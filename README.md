# wetype-linux

微信输入法引擎在 Linux 上的接入层。引擎是腾讯的 Windows x86-64 二进制
（`libwxhld.dll`），在 Wine 下原生运行。

**函数**只用导出符号名绑定，不硬编码地址。但**数据结构**仍是硬编码布局：
`InitParam`（0xD0）、`SessionConfig`（0x138）、候选结构体的 6 个偏移、
pending-input 节点的 4 个字段。这些不是"随便换版本也能跑"的东西 —— 换引擎
版本时它们才是真正的风险点，`wxime_size_check` 只能挡住前两个。

当前状态：`initialize → config_dict → create_session → 逐字符 process_input
→ 会话监听 → 候选 → select_candidate` 全链路已通，可脱离 fcitx5 用 CLI 验证。

```
crates/
├── wetype-types/    协议消息（prost）+ 帧编解码，3 条链路共用
├── wetype-engine/   引擎宿主，目标 x86_64-pc-windows-gnu，Wine 里跑
├── wetype-daemon/   常驻服务，tokio + Unix socket
├── wetype-client/   socket 客户端：后台读线程 + 超时 + 熔断
├── wetype-ffi/      C ABI：staticlib + cdylib，头文件由 cbindgen 生成
└── wetype-cli/      调试客户端，Linux ELF
frontends/
└── fcitx5/          C++ addon + Meson，通过 C ABI 调用 Rust
scripts/             build.sh、install.sh、uninstall.sh、fetch_wetype.sh
docs/                安装指引（通用流程 + 发行版小节）
wetype.lock          锁定的 WeType 版本、官方 URL 与校验和
```

外部依赖的版本集中在根 `Cargo.toml` 的 `[workspace.dependencies]`，各 crate 写
`foo.workspace = true`。成员之间的 path 依赖则各写各的
（`wetype-types = { path = "../wetype-types" }`）—— 它们没有版本可共享，
放进根目录只是多一层间接。

```
fcitx5 addon --unix socket--> wetype-daemon --stdio pipe--> wetype-engine.exe
```

daemon 是唯一知道 Wine 存在的组件。它还负责 session 归属：每个 session 属于
唯一连接，操作别人的 session 会被拒绝，连接断开时自动关闭它创建的 session。

## 准备引擎载荷

`fetch_wetype.sh` 把腾讯的专有二进制下载到仓库里的 `bin/engine/`（约 270 MB，
gitignored），`install.sh` 再把它装进 `/usr/share/wetype/engine`：

```sh
scripts/fetch_wetype.sh           # 按 wetype.lock
scripts/fetch_wetype.sh --check   # 只查新版本
scripts/fetch_wetype.sh --update  # 拉最新并改写 lock
```

版本、官方 URL 和两处 SHA-256 都在仓库根目录的 `wetype.lock` 里。脚本会校验
安装包和 `libwxhld.dll` 的哈希；换版本时整体替换 `bin/engine`，不会把旧版的
词典混进来。安装包缓存在 `bin/downloads/`。`--update` 会顺带把 lock 里的
版本、URL 和两处哈希改成新值（引擎哈希要等解包后才知道）。

版本发现走 `microsoft/winget-pkgs` 的 `Tencent.WeType` 清单：它同时列出官方
`InstallerUrl` 和 `InstallerSha256`，所以 `--version X` 也能拿到校验和。这个
下载是**手动步骤**，`install.sh` 不会替你执行。

运行时依赖的闭包（脚本会自动取）：
`libwxhld.dll`、`XNet.dll`、`tensorflowlite_c.dll`、`ime_net.dll`、`wcwss.dll`。
用 `objdump -p libwxhld.dll | grep 'DLL Name'` 可以复核。

## 构建

```sh
scripts/build.sh
```

它编 release 的 daemon/CLI、Windows 引擎宿主（复制进 `bin/engine/`，和 DLL 同
目录），再用 Meson 编 fcitx5 插件（顺带重新生成 C 头文件）。Meson 只负责编译，
不声明任何安装规则；`/usr` 下的文件清单全部写在 `install.sh` 里。

检查：

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo machete
```

## 跑一遍端到端

引擎宿主和它的 DLL 必须同目录（Windows 先搜 exe 所在目录），`build.sh` 已经把
宿主放进 `bin/engine/`。先按上一节准备好载荷，然后：

```sh
export WINEPREFIX=$PWD/.run/wineprefix    # 任意可写前缀
export WETYPE_ENGINE_DIR=$PWD/bin/engine
export WETYPE_DATA_DIR=$PWD/.run/data

./target/x86_64-unknown-linux-gnu/release/wetype-cli \
    --engine $WETYPE_ENGINE_DIR/wetype-engine.exe --keys nihao
```

预期输出：

```
engine: 0.45.0.weekly_release/2026.10.26.071b698
session: 1
key 'n' -> gen 1 : 30 candidates, preedit "n"
...
 0. 你好 (cover 5)
 ...
committed "你好" (finished true, preedit "")
```

## 环境变量

| 变量 | 作用 |
|---|---|
| `WETYPE_ENGINE_DIR` | 含 `libwxhld.dll` 与 `BuildIn/` 的目录（默认 `.`） |
| `WETYPE_DLL` | 覆盖 DLL 路径（默认 `$WETYPE_ENGINE_DIR/libwxhld.dll`） |
| `WETYPE_DATA_DIR` | 引擎工作目录，必须可写（默认 `$WETYPE_ENGINE_DIR/data`） |
| `WETYPE_UIN` | 可选，传给 `wxime_reset_user_id` |
| `WETYPE_DICT_RELATIVE` | 设为任意值则词典路径用 `BuildIn/x.bin` 相对形式 |
| `WETYPE_CLOUD` | 设为任意值才开启云候选（默认**关闭**，见下） |
| `WETYPE_LOG` | `debug` / `trace` 提高日志级别（默认 info） |
| `WETYPE_DEBUG_EVENTS` | 设为任意值则转储每个监听事件载荷的第一个 qword（需 `WETYPE_LOG=debug`） |

## 监听事件语义

会话监听是拿到全部状态的唯一通道。实测各类型的含义（`types[i]`）：

| 类型 | 含义 | 载荷 |
|---|---|---|
| `0` | 候选列表 | 首个指针 = 候选迭代器，`wxime_candidate_next` 遍历后必须 `delete_candidate_iterator` |
| `2` | **上屏文本**（引擎要求立刻提交） | `{const char*, u32 len}` |
| `4` | 引擎会发，**官方宿主也忽略** | 无意义载荷，忽略即可 |
| `5` | **组合串（preedit）** | pending-input 单链表，见下 |
| `6` | 云候选结果（引擎已合并） | 同类型 0 的迭代器 |

> 曾经把 `2` 当 preedit 用，是错的 —— 它带的是 `"你"` 这种要上屏的文本。
> 参考实现 `refs/fcitx5-wetypex` 里同样把 `2` 送进 commits 队列。

### pending-input 节点（类型 5）

```text
+0   u32  类型
+8   ptr  下一个节点
+24  u32  长度        (类型 0/1)
+24  u8   原始字节    (类型 2/4/5/6)
+32  ptr  数据        (类型 0/1)
```

类型：`0` 已选文本、`1` 精确匹配文本、`2/4/5` 纠错记录、`6` 分隔符、`7` 光标标记。
纠错记录只取 `+24` 的用户原始字节，引擎合成的替换内容不显示。

实测 `nihao` 的 preedit 是 **`ni'hao`** —— 音节边界的 `'` 由引擎插入，
比我们自己拼拼音准确，所以直接用它。

## 云候选：不做，也不该默认开

引擎自带网络栈（`cronet_network.cpp`，Chromium 的 Cronet），HTTP 全在
`libwxhld.dll` 内部完成 —— **不需要任何外部 HTTP 库**，我们用不到 reqwest。

但云候选在当前形态下是走不通的，实测引擎日志：

```
user_id.h:91        ERR| set uin failed: id[]
cronet_network.cpp  INFO| IsValid:0
"ilink_uin":""
```

没有账号态（`ilink_uin` 为空），Cronet 也报告网络配置无效（`IsValid:0`），
请求发不出去。这属于账号层面的事，不是一个 Linux 输入法该去伪造的。

因此 `SessionConfig+0x41`（`cloud.enable_cloud_search_by_input`）**默认 0**，
与参考实现不同：它会把用户击键送往腾讯服务器，而当前又换不来任何候选。
需要时用 `WETYPE_CLOUD=1` 打开。

接收侧本来就已经支持：事件类型 6 走和类型 0 相同的迭代器路径，
`Candidate.cloud` 由 `marker==0xFFFD && origin==11000` 判定。

## 手动跑

```sh
export WETYPE_ENGINE_DIR=$PWD/bin/engine
export WETYPE_DATA_DIR=$PWD/.run/data

./target/x86_64-unknown-linux-gnu/release/wetype-daemon &   # 日志走 stderr
./target/x86_64-unknown-linux-gnu/release/wetype-cli --keys nihao
```

> 注意产物在 `target/x86_64-unknown-linux-gnu/release/`，不是 `target/release/`
> —— 因为本项目的构建都带了 `--target`。

`--socket PATH` 表示连到已经在跑的 daemon，`PATH` 就是 daemon 打印在启动日志里的
那个 socket 文件（默认 `$XDG_RUNTIME_DIR/wetype.sock`，不可写时退回
`$HOME/.local/state/wetype/wetype.sock`）。不带 `--socket` 时 CLI 自己 spawn 一个
引擎宿主，不需要 daemon。

调试开关：`--pick N`（选第 N 个候选，默认第一个）、`--no-close`（不关 session）、
`--linger N`（保持连接 N 秒）、`--steal ID`（拿别人的 session 试一下，应当被拒绝）。

## fcitx5 插件

从 clone 到装进 `/usr`、启用服务和卸载的完整步骤见
[docs/installation.md](docs/installation.md)。下面是构建和打包视角的摘要。

PKGBUILD 的 `package()` 就是这样调用（`$srcdir`/`$pkgdir` 由包管理器提供）：

```sh
scripts/build.sh
scripts/install.sh --destdir "$pkgdir"
```

直接装进本机 `/usr`：`scripts/build.sh && sudo scripts/install.sh`
（会用当前用户启用服务）。卸载用 `sudo scripts/uninstall.sh`（`--destdir DIR`
只清暂存树）。

`install.sh` 用 `install -Dm` 逐项落位（不再调用 `meson install`），文件清单只
有一份。装完布局：

```
/usr/bin/wetype-daemon
/usr/lib/fcitx5/libwetype.so
/usr/share/fcitx5/addon/wetype.conf          # addon 元数据
/usr/share/fcitx5/inputmethod/wetype.conf    # 输入法条目
/usr/share/wetype/engine/                    # 载荷 + wetype-engine.exe
/usr/lib/systemd/user/wetype-daemon.service
```

包管理不该替用户启用服务：每个用户自己执行
`systemctl --user enable --now wetype-daemon`，再重启 fcitx5 并把「微信输入法」
加进输入法列表。只有插件、没有 daemon 时按键不会被拦截，输入法等于不存在。

可用配置（`fcitx5-configtool` → 附加组件 → 微信输入法）：

- **中英文切换键**：Shift / Ctrl / 两者。
- **预编辑显示方式**：`fcitx5 显示`（默认，拼音画在候选窗口里）或
  `应用内显示`（交给应用自己画，像原生输入法）。只有声明了 `preedit` 能力的
  应用才能用后者，否则自动回退到 `fcitx5 显示`，不会出现「拼音消失」。
- **中文标点总开关**：配置页和状态栏菜单里的「标点映射：中文/英文」是同一个
  开关，菜单里点一下立刻生效并写回配置。
- **逐符号开关**：关掉的键保持 ASCII。`'` 在组合中始终是拼音音节分隔符
  （`xi'an`），只有空闲时才是引号，且引号按开/闭配对。
- **回车键 / 空格键**：各自可选「提交原始输入」或「选第一个候选」，默认分别是
  前者和后者。没有组合时空格照常输入空格，回车照常换行。

C++ 侧只做接口转换：`KeyEvent` → `wetype_*`，回包 → preedit + 候选列表。
按键吃什么、上屏什么，全由引擎决定。

`include/wetype/wetype.h` 由 Rust ABI 生成，**Meson 构建会自动重跑生成器**
（`cargo run --features cbindgen`；内容没变就不落盘，所以不会引起重编译）。
不构建前端、只想刷新仓库里那份时：

```sh
cargo run -p wetype-ffi --features cbindgen --bin wetype-ffi-header
```

## 已知限制

- **应用名固定为 `Weixin.exe`。** 建 session 时没有把 fcitx5 的 `program()` 传过去
  （`wetype_new_session(..., "", ...)`），引擎因此把所有应用都当成微信场景。哪些
  行为真的受它影响还没验证过，所以没有贸然换成 Linux 程序名——未知场景可能比固定
  的微信场景更不可测。
- **部分选词不训练用户词频**，原因和实测见「部分上屏」。

## 退出路径

引擎跑起来之后，`ExitProcess` 会在 loader lock 上死锁（探针阶段反复观测到）。
解决办法是引擎自己的 `wxime_finalize`：

```
wxime_finalize()  ->  引擎停掉工作线程  ->  process::exit(0) 正常完成
```

实测退出码 `0`、整轮 0.27 秒，无需 kill。

**刻意不做 `TerminateProcess` 兜底**：人为看门狗只会掩盖将来的回归；真卡死了
该由上层（daemon / systemd）发现并处理，而不是在受害者进程里再开一个线程。

DLL 句柄仍然**永不卸载**（`ManuallyDrop`）：进程都要结束了，`FreeLibrary`
没有任何收益，只有风险。

## 另一个必须知道的坑

**`process_input` 只接受一个可打印、非空格的 ASCII 字节。** 整串、多字节字符、
非 ASCII 字节都会让引擎抛 C++ 异常（`0xe06d7363`），而且异常会杀掉**整个**引擎
进程，连带所有 session。空格（`0x20`）是唯一一个例外：它同样是可打印 ASCII，
但引擎收到一样会抛异常，所以协议 `Key.utf8` 只允许 `0x21..=0x7E` 的单字节，
引擎宿主会拒绝其余请求。回车和空格由前端自己处理（上屏），从不发给引擎。

## 引擎监督

`wetype-daemon` 是监督者：每 500ms 检查引擎子进程，一旦它自己死了，
daemon 清理 socket 并以非零码退出，交给 systemd 重启。它同时处理 SIGTERM
（systemd 默认信号），并在启动时拒绝抢占已被占用的 socket。

## 部分上屏

候选自带的 `cover`（候选 +0x64）是 **raw 拼音的字符数**，所以「只选前几个字」是
可以判定的，不需要猜 `is_pre`/`suffix_len`：

1. 选中的候选只覆盖一部分时，引擎宿主先提交这段，`reset_session`，再把剩下的
   拼音**逐字节重新 `process_input`** 进去；
2. 回包里的 `rest` 是这段新组合的普通快照（preedit / raw / 候选），前端不需要
   自己拼拼音；
3. preedit 只显示**还在组合**的那部分，已提交的文字属于应用、不再重复显示。
   这也是 Backspace 能删掉已上屏文字的前提：拼音退完后 preedit 变空，下一个
   Backspace 直接交给应用。

例：`nihaoshijie` 里选「你好」（cover 5）→ 应用上屏「你好」，preedit 只剩
`shi'jie`，候选列表刷新成它的。

**已知限制：部分选词不训练用户词频。** 整词选择会立刻改变下次的排序，并且跨引擎
重启保留（`WETYPE_DATA_DIR` 下的 `user_dict/`、`user_hot_word/`）；部分选词实测
不改变排序，因为它对引擎来说是「选择 + 重置 + 重新输入」，从没有一次覆盖整串的
选择。引擎自己的 `is_pre`/`suffix_len` 参数看起来正是为此准备的，但探针表明
`is_pre` 无法从 `select_candidate` 现有的 12 个参数传入（`suffix_len` 能传，
单独传却没有效果），refs 也全部传 0，所以短期内保持现状。

协议上是 `SelectResult.rest`；旧 daemon 没有这个字段，新插件只会退化成整词上屏，
不会解析错。

## systemd 用户服务

`scripts/install.sh` 把 `packaging/systemd/wetype-daemon.service.in` 渲染到
`/usr/lib/systemd/user/`，但不 enable——启用是每个用户自己的事：

```sh
systemctl --user daemon-reload
systemctl --user enable --now wetype-daemon
```

引擎载荷固定在 `/usr/share/wetype/engine`，可写状态（词典、Wine 前缀）用
`%S` 指到用户的 state 目录（`~/.local/state/wetype`）。几个关键设定：

- `Restart=on-failure` + `RestartSec=3`：引擎死掉时 daemon 以非零码退出，systemd
  重启整对进程；3 秒正好和客户端的重连节奏对齐，所以 fcitx5 不需要跟着重启。
- `KillMode=mixed`：`systemctl stop` 只给 daemon 发 SIGTERM，让它先把引擎的 stdin
  关掉、走完 `wxime_finalize`；如果按默认的 control-group 一起发，Wine 会先死，
  就跳过了收尾。
- `StartLimitIntervalSec=30` / `StartLimitBurst=10`：偶发崩溃能自愈，装坏了也不会
  无限重启刷日志。
- 不设 `WETYPE_SOCKET`：默认就是 `$XDG_RUNTIME_DIR/wetype.sock`，用户管理器已经
  设好了，插件算出来的也是同一个路径。

## 断线重连

插件不把 daemon 的可用性当前提。`wetype-client` 有一个 supervisor 线程持有
连接：读回包、发现对端关闭、并在每 3 秒重试一次连接——不退回、不在
`keyEvent` 里等。

- 断开时打一行 `daemon connection lost, retrying every 3000 ms`，之后静默；
- 连上再打一行 `connected to ... (generation N)`；
- 每次连接成功都会把 generation 加一。前端拿它和自己创建 session 时记下的值
  比较，不同就说明 daemon 重启过、所有 session id 都作废了，于是丢掉旧 session，
  下一次按键自动重建；
- daemon 不在时按键直接透传给应用（等于英文输入），不会卡住桌面。

界面上也能看出来：连不上 daemon 时，切换中英文的提示会变成
`微（服务缺失）` / `abc（服务缺失）`；这时直接打字的话，第一次建 session
失败也会弹一次同样的提示（每次断线只弹一次），恢复连接后自动恢复正常文案。

日志只在这条路径上出现：daemon 起晚了是常见场景，没有这两行日志根本分不清
「插件没加载」和「daemon 没起来」。

注意这个提示只看 socket 状态，所以能识别「daemon 没起来 / 挂了」，识别不了
「socket 还在但引擎卡死」——后者要靠 daemon 自己的监督和 systemd。
