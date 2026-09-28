// セレクタに一致した要素の HTML を取り出す。src/main.rs の extract_html_fragments が
// (この関数式)(セレクタ, 見えない要素も残すか, 無効なセレクタのときの番兵) の形でページの中で評価する。
//
// 戻り値は JSON の文字列 { matched, fragments, hiddenElements, hiddenTexts, fadedChars, keptChars }。
// fadedChars は opacity: 0 と visibility: hidden のために除いた文字の数 (空白を除く) で、
// スクロールで表示するアニメーションの前の本文を除きすぎていないかの目安にする。
// 無効なセレクタのときは番兵に例外のメッセージを続けた文字列を、見えるかの判定の途中で例外が
// 起きたときは { error } を返す。
//
// 見えるかは生の DOM の計算済みスタイルで判定し (計算済みスタイルは生の DOM でしか読めない)、
// 要素の複製から見えないものを削ってから outerHTML にする。複製は閲覧のコンテキストを持たない
// 文書に importNode したもので、画像の読み込みやカスタム要素の構築は起きない。生の DOM と複製は
// 子ノードの位置で対応させて走査する。判定は取得した時点の表示を基準にする (閉じたタブや
// details の中身、スクロールで表示するアニメーションの前の要素は除かれる)。
(function extractFragments(selector, keepHidden, errorSentinel) {
  'use strict';

  let roots;
  try {
    roots = Array.from(document.querySelectorAll(selector));
  } catch (err) {
    return errorSentinel + messageOf(err);
  }

  if (keepHidden) {
    return JSON.stringify({
      matched: roots.length,
      fragments: roots.map((root) => root.outerHTML),
    });
  }

  try {
    return JSON.stringify(extractVisible(roots));
  } catch (err) {
    return JSON.stringify({ error: messageOf(err) });
  }

  function messageOf(err) {
    return String(err && err.message ? err.message : err);
  }

  function extractVisible(roots) {
    // htmd が捨てる要素と、もともと描画されない要素。見えるかを判定せず、数えもせずにそのまま残す
    const UNJUDGED = new Set([
      'script',
      'style',
      'noscript',
      'template',
      'svg',
      'head',
      'title',
      'meta',
      'link',
      'base',
    ]);

    const inert = document.implementation.createHTMLDocument('');
    const styles = new Map();
    const textVisibility = new Map();
    const textClips = new Map();

    // ページの外への配置は、スクロールしても見えない左か上の外だけを除く。横書きのときだけ上を、
    // さらに左から右へ書くときだけ左を判定する (右から左や縦書きでは左や上へスクロールできる)。
    // 文書の書字方向は body の値が使われる
    const pageStyle = styleOf(document.body || document.documentElement);
    const horizontal = pageStyle.writingMode === 'horizontal-tb';
    const checkTop = horizontal;
    const checkLeft = horizontal && pageStyle.direction === 'ltr';

    const result = {
      matched: roots.length,
      fragments: [],
      hiddenElements: 0,
      hiddenTexts: 0,
      fadedChars: 0,
      keptChars: 0,
    };
    let inHiddenTextRun = false;

    for (const root of roots) {
      if (UNJUDGED.has(root.localName)) {
        result.fragments.push(root.outerHTML);
        continue;
      }
      const reason = rootHiddenReason(root);
      if (reason === null) {
        result.fragments.push(copyVisible(root));
      } else {
        // 選んだ要素そのものが見えなければ、その断片を出さない
        recordHiddenElement(root, reason);
      }
    }
    return result;

    function copyVisible(root) {
      const copy = inert.importNode(root, true);
      inHiddenTextRun = false;
      visit(root, copy);
      return copy.outerHTML;
    }

    function visit(original, copy) {
      const originals = original.childNodes;
      const copies = Array.from(copy.childNodes);
      if (originals.length !== copies.length) {
        throw new Error('the copy does not match the original element');
      }
      for (let i = 0; i < originals.length; i++) {
        const node = originals[i];
        if (node.nodeType === Node.ELEMENT_NODE) {
          if (UNJUDGED.has(node.localName)) {
            continue;
          }
          const reason = hiddenReason(node);
          if (reason === null) {
            // display: contents や、子で visibility を戻す場合があるので、子も 1 つずつ見る
            visit(node, copies[i]);
          } else {
            copies[i].remove();
            recordHiddenElement(node, reason);
          }
        } else if (node.nodeType === Node.TEXT_NODE) {
          const length = visibleLength(node.data);
          if (length === 0) {
            // 空白だけのテキストは判定せずに残す
            continue;
          }
          const reason = textHiddenReason(node);
          if (reason === null) {
            result.keptChars += length;
            inHiddenTextRun = false;
          } else {
            copies[i].remove();
            if (reason === 'visibility') {
              result.fadedChars += length;
            }
            if (!inHiddenTextRun) {
              result.hiddenTexts += 1;
            }
            inHiddenTextRun = true;
          }
        }
      }
    }

    // 子孫ごと除いた要素を記録する。数えるのは文字か画像を含む要素だけ
    // (hidden の input・空の装飾・アイコンを数えない)
    function recordHiddenElement(element, reason) {
      const length = subtreeTextLength(element);
      if (reason === 'opacity') {
        result.fadedChars += length;
      }
      if (length > 0 || element.localName === 'img' || element.querySelector('img') !== null) {
        result.hiddenElements += 1;
      }
    }

    function visibleLength(text) {
      return text.replace(/\s+/g, '').length;
    }

    function subtreeTextLength(root) {
      let length = 0;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {
        acceptNode: (node) =>
          node.nodeType === Node.ELEMENT_NODE && UNJUDGED.has(node.localName)
            ? NodeFilter.FILTER_REJECT
            : NodeFilter.FILTER_ACCEPT,
      });
      while (walker.nextNode()) {
        const node = walker.currentNode;
        if (node.nodeType === Node.TEXT_NODE) {
          length += visibleLength(node.data);
        }
      }
      return length;
    }

    function styleOf(element) {
      let style = styles.get(element);
      if (style === undefined) {
        style = getComputedStyle(element);
        styles.set(element, style);
      }
      return style;
    }

    // 選んだ要素そのものか、その祖先 (シャドウツリーを含めた表示の木で) が子孫ごと見えないなら、
    // その理由を返す
    function rootHiddenReason(root) {
      for (let element = root; element; element = flatParent(element)) {
        const reason = hiddenReason(element);
        if (reason !== null) {
          return reason;
        }
      }
      return null;
    }

    function flatParent(element) {
      if (element.assignedSlot) {
        return element.assignedSlot;
      }
      const parent = element.parentNode;
      if (parent instanceof ShadowRoot) {
        return parent.host;
      }
      return parent instanceof Element ? parent : null;
    }

    // 要素を子孫ごと除くなら、その理由を返す (除かないなら null)。hidden 属性や aria-hidden
    // そのものでは判定しない (hidden は CSS で上書きされうるので計算済みの display で見る。
    // aria-hidden は画面の表示を変えない)
    function hiddenReason(element) {
      const style = styleOf(element);
      if (style.display === 'none') {
        return 'display';
      }
      if (isClosedDetailsContent(element)) {
        return 'details';
      }
      if (style.display === 'contents') {
        // 箱を作らないので opacity などは効かず、子は表示される
        return null;
      }
      if (style.contentVisibility === 'hidden') {
        return 'content-visibility';
      }
      // 祖先の opacity: 0 は、その祖先の段階で除いている
      if (Number.parseFloat(style.opacity) === 0) {
        return 'opacity';
      }
      if (style.position === 'absolute' || style.position === 'fixed') {
        const rect = element.getBoundingClientRect();
        if (isVisuallyHidden(style, rect)) {
          return 'clip';
        }
        if (isOutsidePage(element, style.position, rect)) {
          return 'outside';
        }
      }
      return null;
    }

    // 閉じた details の、最初の summary 以外の子か
    function isClosedDetailsContent(element) {
      const parent = element.parentElement;
      return (
        parent instanceof HTMLDetailsElement && !parent.open && element !== summaryOf(parent)
      );
    }

    function summaryOf(details) {
      for (const child of details.children) {
        if (child.localName === 'summary') {
          return child;
        }
      }
      return null;
    }

    // 支援技術向けに画面から隠す形 (visually-hidden): 絶対配置で、clip が面積 0 の矩形か
    // clip-path が全体を切り取る inset で、大きさが 1px 以下
    function isVisuallyHidden(style, rect) {
      if (rect.width > 1 || rect.height > 1) {
        return false;
      }
      return clipIsEmpty(style.clip) || clipPathHidesAll(style.clipPath, rect);
    }

    function clipIsEmpty(clip) {
      const match = /^rect\((.*)\)$/.exec((clip || '').trim());
      if (!match) {
        return false;
      }
      const edges = match[1].trim().split(/\s*,\s*|\s+/);
      if (edges.length !== 4) {
        return false;
      }
      const [top, right, bottom, left] = edges.map((edge) => lengthInPixels(edge));
      return (
        (top !== null && bottom !== null && bottom <= top) ||
        (left !== null && right !== null && right <= left)
      );
    }

    function clipPathHidesAll(clipPath, rect) {
      const match = /^inset\((.*)\)$/.exec((clipPath || '').trim());
      if (!match) {
        return false;
      }
      const values = match[1].split(/\s+round\s+/)[0].trim().split(/\s+/);
      if (values.length < 1 || values.length > 4) {
        return false;
      }
      const [t, r = t, b = t, l = r] = values;
      const top = lengthInPixels(t, rect.height);
      const right = lengthInPixels(r, rect.width);
      const bottom = lengthInPixels(b, rect.height);
      const left = lengthInPixels(l, rect.width);
      if ([top, right, bottom, left].includes(null)) {
        return false;
      }
      return top + bottom >= rect.height || left + right >= rect.width;
    }

    // px と % (基準の大きさがあるとき) の長さを px にする。読めなければ null
    function lengthInPixels(value, size) {
      const text = value.trim();
      if (text === '0') {
        return 0;
      }
      const number = Number.parseFloat(text);
      if (Number.isNaN(number)) {
        return null;
      }
      if (text.endsWith('px')) {
        return number;
      }
      if (text.endsWith('%') && size !== undefined) {
        return (number * size) / 100;
      }
      return null;
    }

    // ページの左か上の外に完全に出ているか。はみ出した子孫が 1 つでもページの中にあれば出ていない
    function isOutsidePage(element, position, rect) {
      if (!checkTop && !checkLeft) {
        return false;
      }
      const scrollX = position === 'fixed' ? 0 : window.scrollX;
      const scrollY = position === 'fixed' ? 0 : window.scrollY;
      const outside = (box) =>
        (checkLeft && box.right + scrollX <= 0) || (checkTop && box.bottom + scrollY <= 0);
      if (!outside(rect)) {
        return false;
      }
      const range = document.createRange();
      range.selectNodeContents(element);
      for (const box of range.getClientRects()) {
        if ((box.width > 0 || box.height > 0) && !outside(box)) {
          return false;
        }
      }
      return true;
    }

    // テキストが見えないなら、その理由を返す (見えるなら null)。親の要素の計算済みスタイルで
    // 判定し、親ごとに覚えておく
    function textHiddenReason(text) {
      const parent = text.parentElement;
      if (!parent) {
        return null;
      }
      if (!textVisibility.has(parent)) {
        textVisibility.set(parent, computeTextHiddenReason(parent));
      }
      return textVisibility.get(parent);
    }

    function computeTextHiddenReason(element) {
      // 閉じた details の直下の文字は summary の外なので表示されない
      if (element instanceof HTMLDetailsElement && !element.open) {
        return 'details';
      }
      const style = styleOf(element);
      // visibility は子で visible に戻せるので、子孫ごとではなくテキストごとに見る
      if (style.visibility === 'hidden' || style.visibility === 'collapse') {
        return 'visibility';
      }
      if (Number.parseFloat(style.fontSize) === 0) {
        return 'font-size';
      }
      const fill = style.getPropertyValue('-webkit-text-fill-color') || style.color;
      if (alphaOf(fill) > 0) {
        return null;
      }
      // 塗りが透明でも、縁取り・影・文字の形に切り抜いた背景 (グラデーションの見出しなど) で
      // 字形が見える。描画を確かめられない値は見えるとみなす
      if (
        Number.parseFloat(style.getPropertyValue('-webkit-text-stroke-width')) > 0 &&
        alphaOf(style.getPropertyValue('-webkit-text-stroke-color')) > 0
      ) {
        return null;
      }
      if (hasVisibleShadow(style.textShadow) || hasBackgroundClippedToText(element)) {
        return null;
      }
      return 'color';
    }

    function hasBackgroundClippedToText(element) {
      for (let node = element; node; node = flatParent(node)) {
        let clipped = textClips.get(node);
        if (clipped === undefined) {
          const style = styleOf(node);
          const clips = `${style.backgroundClip},${style.getPropertyValue('-webkit-background-clip')}`;
          clipped =
            clips.split(',').some((clip) => clip.trim() === 'text') &&
            (style.backgroundImage !== 'none' || alphaOf(style.backgroundColor) > 0);
          textClips.set(node, clipped);
        }
        if (clipped) {
          return true;
        }
      }
      return false;
    }

    function hasVisibleShadow(value) {
      if (!value || value === 'none') {
        return false;
      }
      for (const layer of splitTopLevel(value)) {
        const color = /[a-z-]+\([^)]*\)|transparent/i.exec(layer);
        if (!color || alphaOf(color[0]) > 0) {
          return true;
        }
      }
      return false;
    }

    function splitTopLevel(value) {
      const parts = [];
      let depth = 0;
      let start = 0;
      for (let i = 0; i < value.length; i++) {
        const ch = value[i];
        if (ch === '(') {
          depth += 1;
        } else if (ch === ')') {
          depth -= 1;
        } else if (ch === ',' && depth === 0) {
          parts.push(value.slice(start, i));
          start = i + 1;
        }
      }
      parts.push(value.slice(start));
      return parts;
    }

    // 計算済みの色の不透明度。読めない書式は不透明とみなす
    function alphaOf(color) {
      const value = (color || '').trim().toLowerCase();
      if (value === '') {
        return 1;
      }
      if (value === 'transparent') {
        return 0;
      }
      const slash = /\/\s*([+-]?[\d.]+(?:e[+-]?\d+)?%?)\s*\)$/.exec(value);
      if (slash) {
        return parseAlpha(slash[1]);
      }
      const legacy = /^(?:rgba|hsla)\(([^)]*)\)$/.exec(value);
      if (legacy) {
        const parts = legacy[1].split(',');
        if (parts.length === 4) {
          return parseAlpha(parts[3].trim());
        }
      }
      return 1;
    }

    function parseAlpha(text) {
      const number = Number.parseFloat(text);
      if (Number.isNaN(number)) {
        return 1;
      }
      return text.endsWith('%') ? number / 100 : number;
    }
  }
})
