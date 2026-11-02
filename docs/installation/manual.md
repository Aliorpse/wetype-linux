# 手动安装

## 0. 依赖

构建期：

- Rust 工具链（需要 `x86_64-pc-windows-gnu` target）与 mingw-w64 链接器
- GCC、`pkg-config`、Meson、Ninja
- Fcitx5 开发头文件
- 7-Zip（第 2 步解包官方安装器）

运行期：

- Wine
- Fcitx5

## 1. 克隆

```sh
git clone https://github.com/Aliorpse/wetype-linux
cd wetype-linux
```

## 2. 下载输入法引擎

引擎二进制不随仓库分发。`fetch_wetype.sh` 按 `wetype.lock` 的版本从官方地址下载并校验，解包到 `bin/engine/`：

```sh
./scripts/fetch_wetype.sh
```

## 3. 构建

```sh
./scripts/build.sh
```

产物：

- `./target/x86_64-unknown-linux-gnu/release/wetype-daemon`
- `./bin/engine/wetype-engine.exe`
- `./frontends/fcitx5/build/libwetype.so`

## 4. 安装

```sh
sudo ./scripts/install.sh
```

## 5. 检查服务

```sh
systemctl --user status wetype-daemon
```

引擎的词典和 Wine 前缀写在 `~/.local/state/wetype/`（unit 里的 `%S`），`/usr/share/wetype/engine` 保持只读。

## 6. 在 fcitx5 里启用

重启 fcitx5，然后在配置工具里把「微信输入法」加进输入法列表：

```sh
fcitx5 -r -d
fcitx5-configtool
```

## 7. 卸载

```sh
sudo ./scripts/uninstall.sh
```
