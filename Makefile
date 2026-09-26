# get-md の開発タスク。引数なしの `make` でターゲット一覧を表示する。
#
# ツールの版は mise.toml に固定する。mise が使える場合はすべて `mise exec --` 経由で
# 実行し、IDE や GUI から起動してシェルで mise を有効化していなくても固定版を使う。
# SYSTEM_TOOLS=1 では PATH 上のツールを使うため、版は保証されない。
#
# macOS 標準の GNU Make 3.81 で使える機能だけを使う。
# .ONESHELL、.SHELLFLAGS、$(file ...) と != は使わない。

.DEFAULT_GOAL := help

BINARY_NAME := get-md
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock を追跡しているため、CI と同じ依存を解決する。
CARGO_FLAGS ?= --locked

# ---- ツールチェーン --------------------------------------------------------------
# mise を PATH と標準的なインストール先から探す。GUI から起動した make にはシェルの
# PATH が渡らないことがある。明示する場合は make MISE=/path/to/mise を使う。
# mise がない場合の挙動を試すには MISE_CANDIDATES= で候補を空にする。
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise was not found. Install it from https://mise.jdx.dev, or add SYSTEM_TOOLS=1 to use the tools on PATH)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: help setup build release run test test-e2e lint fmt fmt-check check ci install uninstall clean

## 準備

setup: ## ツールチェーンと依存を取得する
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## ビルド

build: ## デバッグ版をビルドする
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## リリース版をビルドする
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## デバッグ版を実行する（引数は ARGS="..."）
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

## 検査

# tests/e2e.rs は Chrome/Chromium が必要なため #[ignore] を付ける。
# make test はビルドのみ、make test-e2e は実行まで行う。
test: ## テストを実行する
	$(RUN) cargo test $(CARGO_FLAGS)

test-e2e: ## Chrome/Chromium を使う E2E テストを実行する（CI 対象外）
	$(RUN) cargo test $(CARGO_FLAGS) --test e2e -- --ignored

lint: ## clippy を警告ゼロで実行する
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings

fmt: ## コードを整形する（書き換える）
	$(RUN) cargo fmt --all

fmt-check: ## 整形済みか確認する（書き換えない）
	$(RUN) cargo fmt --all -- --check

check: fmt-check lint ## 整形と静的検査を実行する（書き換えない）

ci: check test ## CI と同じ検査を実行する（書き換えない）

## インストール

# バイナリは直接上書きせず、一時ファイルから rename で置き換える。macOS はコード署名の
# 検証結果を inode ごとにキャッシュするため、実行中または直前に実行したバイナリへ
# cp で上書きすると起動直後に SIGKILL（終了コード 137）で止まる。同じディレクトリ
# 内の一時ファイルを rename すれば inode ごと入れ替わる。
install: release ## リリース版を INSTALL_PATH（既定 /usr/local/bin）へ入れる
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## INSTALL_PATH からバイナリを削除する
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## ビルド成果物を削除する
	$(RUN) cargo clean

## ヘルプ

help: ## この一覧を表示する
	@echo "$(BINARY_NAME) の開発タスク"
	@echo ""
	@echo "使い方: make <target>"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "ツールの版は mise.toml に固定しています。先に make setup を実行してください。"
	@echo "リリース: GitHub Actions > Release > Run workflow"
