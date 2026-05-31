.PHONY: build release install clean test fmt check help

# デフォルトターゲット
.DEFAULT_GOAL := help

# 変数
BINARY_NAME := get-md
INSTALL_PATH := /usr/local/bin

## ビルドコマンド

build: ## デバッグビルドを実行
	cargo build

release: ## リリースビルドを実行
	cargo build --release

## インストール

install: release ## リリースビルドして /usr/local/bin に配置
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/

## 開発

test: ## テストを実行
	cargo test

fmt: ## コードを整形
	cargo fmt

check: ## フォーマット確認、Clippy、cargo check を実行
	cargo fmt -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo check --all-targets --all-features

clean: ## ビルド成果物を削除
	cargo clean

## ヘルプ

help: ## このヘルプを表示
	@echo "$(BINARY_NAME) ビルドコマンド"
	@echo ""
	@echo "使い方: make [target]"
	@echo ""
	@echo "ターゲット:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "リリース:"
	@echo "  GitHub Actions > Release > Run workflow を使用"
