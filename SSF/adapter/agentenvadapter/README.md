# AgentENV adapter

独立适配工程：`agentenvadapter control|node|ctl`。控制面嵌入 SSF Consumption，节点 sidecar 接入 AgentENV 外部块设备协议；原生 Kubernetes 管理 PVC/PV，原生 AgentENV 继续管理 OverlayBD/ublk。

- [统一设计、实现和验收记录](../../../design/12-agentenvadapter-design-and-implementation.md)
- [端到端安装部署手册](../../../design/13-agentenvadapter-installation-and-deployment.md)：按依赖顺序部署、模板构建、真实读写验收与运维。
- [真实环境完整测试方案](../../../design/14-agentenvadapter-real-environment-test-plan.md) · [场景测试工具](tests/realenv/README.md)
- [部署说明](deploy/README.md) · [HTTP API](api/openapi.json) · [Python 客户端](examples/client.py)
- [补丁及基线](patches/manifest.json) · [真实 VM 验收](tests/vm-e2e.py) · [报告模板](tests/report-template.json)

从 SSF 根目录执行：

```sh
go build ./adapter/agentenvadapter/cmd/agentenvadapter
go test ./adapter/agentenvadapter/...
go test -race ./adapter/agentenvadapter/...
python3 adapter/agentenvadapter/scripts/render.py > /tmp/agentenvadapter.json
```

首次部署运行 `agentenvadapter ctl -config CONFIG -action migrate`；它同时迁移共享 SSF 账本和独立适配元数据表。生产需要持久 PostgreSQL、持久 bbolt、mTLS、专用节点池及 SSF Guard。

API 创建和删除返回 `202`。仅 `Ready` 表示 guest 核验及消费提交已完成；仅 `Released` 表示设备、attachment 和计算预留均已释放。错误或超时可继续查询原操作，不能据此删除 PVC 或强制解锁。

当前不支持：带外部卷的暂停恢复、快照、fork、在线扩容及 FileTree。非 Linux 上真实 Runtime 能力返回不支持。macOS 单测和交叉构建不等于 AgentENV/Firecracker 端到端验收。
