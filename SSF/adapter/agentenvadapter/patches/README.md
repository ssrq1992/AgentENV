# AgentENV 通用扩展补丁

基线与 SHA256 见 [manifest.json](manifest.json)，许可见 [AgentENV-LICENSE](AgentENV-LICENSE)。源码已直接修改；补丁用于新 checkout 的重放和审查，不应在当前工作区再执行一次。

```sh
git -C AgentENV checkout 6cccaa7842bd5be2051111d4f74d9e37aa721244
git -C AgentENV apply --check ../SSF/adapter/agentenvadapter/patches/agentenv-v0.2.3-external-storage.patch
git -C AgentENV apply ../SSF/adapter/agentenvadapter/patches/agentenv-v0.2.3-external-storage.patch
```

`patch-bundle.py` 从干净基线归档重放并比对所有修改文件 hash，不写 Git index。本轮只修改 6 个已有 Rust 文件并新增 1 个通用模块：

| 位置 | 修改原因 |
| --- | --- |
| cfg.rs | 可选 Provider、专用节点授权开关、日志路径、只读槽位 |
| custom_extension/mod.rs + storage.rs | 通用设备类型、prepare/launch/ready/stop、fsynced 生命周期日志 |
| firecracker/config.rs | 模板槽位属性读取与默认兼容 |
| firecracker/sandbox.rs | 冷启动/模板设备接入、guest 核验、停止确认；不交给 OverlayBD 生命周期 |
| sandbox/manifest.rs | 保存实际只读槽位，旧模板默认 0 |
| orchestrator/service.rs | 禁止外部卷不支持的生命周期；超时及关机选择停止 |

没有修改 AgentENV Cargo 依赖、API 生成代码、OverlayBD/ublk 模块或原部署文件。适配工程没有复制 Substrate 源码；复用了本项目已有的 SSF 公共实现。MIT 版权及许可证保留。关闭扩展时必须运行 Linux 原有 Rust 回归；本机语法解析检查不代替该回归。
