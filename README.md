# wetype-linux

> [!WARNING]
> 个人项目，作者相关领域知识较弱，代码主要由 LLM 生成。

Linux 微信输入法兼容层，微信输入法的 Windows 引擎通过 Wine 原生加载。

## 功能

- 接入框架
  - [x] Fcitx5
  - [ ] IBus
- 输入方式
  - [x] 拼音
  - [ ] 双拼
  - [ ] 五笔
- [x] 候选、整词上屏、部分上屏
- [x] 中文标点映射
- [x] 用户词频 (限制: 部分上屏暂时无法学习)
- [x] 预编辑：候选窗口内 / 应用内
- [ ] 智能拼写与云候选

### 无计划支持

- 跨设备剪贴板同步：KDE Connect 拥有这个功能。
- 语音输入：过于繁重。
- 隔空传送、V 模式、微信表情、问 AI：超越了输入法的范畴。

需要此类功能的用户，可以查看 [这个项目](https://github.com/panxuc/fcitx5-wetypex)。

## 安装

见 [docs/installation/main.md](docs/installation/main.md)。

## 实现细节

见 [docs/architecture.md](docs/architecture.md)。

## 贡献

见 [docs/contributing.md](docs/contributing.md)。

## 免责声明

本项目与腾讯无关，不包含、也不分发微信输入法的任何二进制。安装脚本只在你自己的机器上从腾讯官方地址下载引擎，并按 `wetype.lock` 校验 SHA-256。

微信输入法是腾讯的专有软件和商标；在 Wine 下运行专有 Windows 输入法存在崩溃和兼容性风险。

本项目基于 [MIT License](LICENSE)。
