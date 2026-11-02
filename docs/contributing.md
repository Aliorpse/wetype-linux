# 贡献

## 开发环境

请参考 [installation/manual.md](installation/manual.md)。

- 本地验证插件：

```sh
./scripts/build.sh
sudo ./scripts/install.sh
fcitx5 -r -d
```

- 不想写 `/usr` 时用 `./scripts/install.sh --destdir /tmp/root`。

## 代码结构

各 crate 的职责、进程模型、引擎 ABI 和已知限制请参考 [architecture.md](architecture.md)。

新的输入法前端从 `crates/wetype-ffi` 的 C ABI 接入，daemon 和引擎侧的改动对所有前端同时生效。

## 引擎版本

`wetype.lock` 存储了当前使用的微信输入法引擎信息。如需更新：

```sh
./scripts/fetch_wetype.sh --update
```

这会改写 `wetype.lock`，并把 `Cargo.toml` / `meson.build` 里的项目版本同步成
`<semver>+wetype<引擎版本>`（`Cargo.lock` 一并刷新）。如需提交引擎版本改动，请
单独提交 PR：

```
chore(update): Bump WeType engine from <旧版本号> to <新版本号>
```

新版本的风险在结构体布局，不在导出符号名——换版本后请至少确认 `initialize → create_session → process_input → 候选 → 上屏` 全链路仍然可用。

## 提交前检查

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo machete
```

C ABI 的头文件由 Meson 构建自动生成，不需要手动改。单独刷新用：

```sh
cargo run -p wetype-ffi --features cbindgen --bin wetype-ffi-header
```

## 提交

- 引擎本体是腾讯的专有二进制，**不要**让它出现在 PR 里；
- 行为变化同步文档：README 的功能表、`architecture.md`、安装文档；
- 可以用 LLM 辅助，但请自己 review，确认代码和文档都能解释得通。
