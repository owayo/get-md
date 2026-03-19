.PHONY: build release install clean test fmt check help

# デフォルトターゲット
.DEFAULT_GOAL := help

# 変数
BINARY_NAME := get-md
INSTALL_PATH := /usr/local/bin

## ビルドコマンド

build: ## Build debug version
	cargo build

release: ## Build release version
	cargo build --release

## インストール

install: release ## Build release and install to /usr/local/bin
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/

## 開発

test: ## Run tests
	cargo test

fmt: ## Format code
	cargo fmt

check: ## Run clippy and check
	cargo clippy -- -D warnings
	cargo check

clean: ## Clean build artifacts
	cargo clean

## ヘルプ

help: ## Show this help message
	@echo "$(BINARY_NAME) Build Commands"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Release:"
	@echo "  Use GitHub Actions > Release > Run workflow"
