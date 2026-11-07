# 架构

## 组件

| 组件 | 职责 |
|---|---|
| `crates/wetype-types` | protobuf 消息与帧编解码 |
| `crates/wetype-engine` | Windows 宿主，在 Wine 里加载 `libwxhld.dll` |
| `crates/wetype-daemon` | 常驻服务：socket、session 归属、引擎监督 |
| `crates/wetype-client` | socket 客户端：后台读线程、超时、重连 |
| `crates/wetype-ffi` | C ABI |
| `crates/wetype-cli` | 调试客户端，不经过 fcitx5 |
| `frontends/fcitx5` | C++ addon：按键映射与界面 |

```text
fcitx5 addon --unix socket--> wetype-daemon --stdio--> wetype-engine.exe
```

daemon 是唯一知道 Wine 存在的组件。

## 事实来源

| 事实 | 位置 |
|---|---|
| 引擎版本、官方 URL、哈希 | `wetype.lock` |
| 协议消息 | `crates/wetype-types/proto/wetype/ipc/ipc.proto` |
| C ABI | `crates/wetype-ffi/src/lib.rs`；头文件由 cbindgen 生成 |
| 安装文件清单 | `scripts/install.sh` |
| systemd 单元 | `packaging/systemd/wetype-daemon.service.in` |
| 引擎常量 | `crates/wetype-engine/src/`（如 `MAX_APP_NAME`） |

## 引擎绑定

- 函数：按导出符号名绑定，不硬编码地址；
- 结构体：硬编码布局——`InitParam` 0xD0、`SessionConfig` 0x138、候选 6 个偏移、pending-input 4 个字段；
- `wxime_size_check` 只覆盖前两个；
- 换版本的风险在结构体布局，不在符号名；
- 版本来自 `wetype.lock`，`build.rs` 注入 `APP_VERSION`。

## IPC

- 插件 ↔ daemon：Unix socket；默认 `$XDG_RUNTIME_DIR/wetype.sock`，不可写时退回 `$HOME/.local/state/wetype/wetype.sock`；
- daemon ↔ 宿主：stdin/stdout 管道；
- 帧：prost + length-delimited，单帧上限与协议版本定义在 `wetype-types`；
- session 归属创建它的连接：跨连接操作被拒，断连自动关闭；
- 单 daemon 上限 256 个 session；
- 会话的 `app_name` 由插件传入 `ic.program()`，空值回退 `Weixin.exe`（`DEFAULT_APP`）。

## 会话事件

| `types[i]` | 含义 | 载荷 |
|---|---|---|
| 0 | 候选 | 迭代器；遍历后必须 `delete_candidate_iterator` |
| 2 | 上屏文本 | `{const char*, u32 len}` |
| 4 | 引擎会发，官方宿主也忽略 | 忽略 |
| 5 | preedit | pending-input 链表 |
| 6 | 云候选（引擎已合并） | 同 0 |

类型 2 是提交，不是 preedit。

pending-input 节点：

```text
+0   u32  类型
+8   ptr  下一个
+24  u32  长度（类型 0/1）/ u8 原始字节（2/4/5/6）
+32  ptr  数据（类型 0/1）
```

类型：0 已选文本、1 精确匹配、2/4/5 纠错记录、6 分隔符、7 光标。纠错记录只取 `+24` 的用户原始字节。类型 0 是已经部分上屏的前缀，单独记录，不再进入 preedit/raw；preedit/raw 只含尚未提交的后缀。

preedit 直接用引擎给的 `ni'hao` 形式，不自己拼拼音。

## 输入约束

- `process_input` 只接受单个可打印、非空格 ASCII 字节（`0x21..=0x7E`）；
- 其余输入（整串、多字节、空格）会让引擎抛 C++ 异常，杀死整个引擎；
- 回车、空格由前端处理，不发给引擎。

## 上屏

- `cover`（候选 +0x64）是 **raw 后缀** 的字符数，据此判断部分上屏；
- 部分上屏：调用 `select_candidate` 后**不** `reset_session`。引擎把已选词保留为 pending-input 的 type-0 节点，并为剩余拼音重新出候选；回包 `SelectResult.rest` 只含剩余的 preedit/raw；
- 前端立即提交已选部分；宿主记下这段前缀。整句完成时引擎通过事件 2 提交整句，宿主扣掉已上屏前缀，只把新的后缀交给前端；
- preedit 只显示尚未提交的后缀，Backspace 不会删除已经上屏的文字；
- 整词和部分上屏都会训练词频。部分上屏的完成过程中，引擎会把部分词和完整短语一起写入 `v5/<uin>/` 用户词库；旧的 reset + 重放流程会让引擎看不到这段前缀，因此不学习。

## 生命周期

- 退出走 `wxime_finalize`（`ExitProcess` 会在 loader lock 死锁），不做 `TerminateProcess` 兜底；DLL 句柄永不卸载（`ManuallyDrop`）；
- daemon 每 500 ms 检查宿主；宿主死亡则清理 socket、以非零码退出；
- 调用超时或协议错位后，连接标记不可用并杀掉引擎，避免迟到的回包被当成下一次响应；
- 插件断线重连：3 秒重试，generation 变化即丢弃旧 session；daemon 不可用时按键透传。

## 云候选

- 引擎内置 Cronet，不需要外部 HTTP 库；
- 无账号态（`ilink_uin` 为空），请求发不出去；
- `SessionConfig+0x41` 默认 0，`WETYPE_CLOUD=1` 才开启；
- 接收侧已支持：类型 6 与类型 0 同一迭代器，`marker==0xFFFD && origin==11000` 标记云候选。

## 智能拼写与模糊音

这些是 `SessionConfig` 的本地字节，不依赖网络，已验证可被 wetype-linux 直接复刻：

| 字节 | 含义 |
|---|---|
| `+0x08..+0x0C` | InputAugmenter：换位、邻键、插入、跳字纠错 |
| `+0x0D` | n/l |
| `+0x0E` | r/l |
| `+0x0F` | h/f |
| `+0x10` | g/k |
| `+0x11` | an/ang |
| `+0x12` | ian/iang |
| `+0x13` | uan/uang |
| `+0x14` | c/ch |
| `+0x15` | s/sh |
| `+0x16` | z/zh |
| `+0x17` | hui/fei |
| `+0x18` | en/eng |
| `+0x19` | in/ing |
| `+0x1A` | on/ong |
| `+0x1B` | huang/wang |
| `+0x1C` | un/ong |
| `+0x1D` | un/iong |
| `+0x1E` | an/ai |
| `+0x1F` | eng/ong |

配置页里是两个分组框：

- **智能纠错**：`启用智能纠错` 总开关 + 换位/邻键/插入/跳字/其他五个子开关，
  默认全开；状态栏/托盘的「智能纠错」菜单项是同一个总开关的快捷入口。
- **模糊音**：19 组独立开关，默认只开 `en/eng` 与 `in/ing`。

这些选项随 `NewSession` 发给 daemon 和 engine host，只在**创建 session** 时写入
`SessionConfig`；修改配置后，下一次按键会关闭旧 session 并按新配置重建。命令行
调试用 `--smart LIST`、`--no-smart`、`--fuzzy LIST` 覆盖（默认与配置页一致：
smart 全开、fuzzy 为 `en_eng,in_ing`）。

## 构建

- `scripts/build.sh`：daemon/CLI、`bin/engine/wetype-engine.exe`、`libwetype.so`；
- Meson 只负责编译；安装清单在 `install.sh`；
- `include/wetype/wetype.h` 由 Meson 调 cbindgen 生成，内容不变不落盘。

## 调试

```sh
export WETYPE_ENGINE_DIR=$PWD/bin/engine
export WETYPE_DATA_DIR=$PWD/.run/data

./target/x86_64-unknown-linux-gnu/release/wetype-daemon &
./target/x86_64-unknown-linux-gnu/release/wetype-cli --keys nihao
```

- 产物在 `target/x86_64-unknown-linux-gnu/release/`（构建带 `--target`）；
- `--socket PATH` 连已有 daemon；不带时 CLI 自己 spawn 宿主；
- `--app NAME` 指定 session 的应用名，用于对比引擎的按应用行为（空即 `Weixin.exe`）；
- `--pick N` 选第 N 个候选，`--no-close`、`--linger N`、`--steal ID` 用于调试会话归属；
- `--smart LIST` / `--no-smart`、`--fuzzy LIST` 覆盖本次会话的智能纠错与模糊音。

## 运行时变量

| 变量 | 作用 |
|---|---|
| `WETYPE_ENGINE_DIR` | 含 `libwxhld.dll` 与 `BuildIn/` 的目录（默认 `.`） |
| `WETYPE_DLL` | 覆盖 DLL 路径（默认 `$WETYPE_ENGINE_DIR/libwxhld.dll`） |
| `WETYPE_DATA_DIR` | 引擎工作目录，必须可写（默认 `$WETYPE_ENGINE_DIR/data`） |
| `WETYPE_UIN` | 传给 `wxime_reset_user_id` |
| `WETYPE_DICT_RELATIVE` | 词典路径用 `BuildIn/x.bin` 相对形式 |
| `WETYPE_CLOUD` | 开启云候选（默认关闭） |
| `WETYPE_LOG` | `debug` / `trace` |
| `WETYPE_DEBUG_EVENTS` | 转储监听事件载荷的第一个 qword（需 `WETYPE_LOG=debug`） |

## 已知限制

- 暂无
