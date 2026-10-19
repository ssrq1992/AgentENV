# 通用 AgentENV 外部设备协议 v1

这一协议位于 AgentENV 扩展边界，内容没有 PVC、CSI 或 SSF 数据库字段。适配服务持有完整签名计划，传入原生创建请求的只有 `customExtensionParams.external_storage.ticket`。用户不能自行提供该字段。

`POST http://127.0.0.1:9445/storage/prepare`：请求为 `RuntimeEvent`（见 `types.go`），包含 sequence=1、ticket、sandbox_id、不可复用的 instance_id、owner_pid/owner_start、boot_id、event=prepare。服务必须已完成 SSF Prepare，且 ticket 未被撤销。响应：

```json
{
  "resources":{"cpu":2,"memory_mib":512},
  "devices":[{"id":"BINDING_HASH","path":"/var/lib/ssf/publish/MANAGED_TARGET","guest_path":"/data","readonly":false,"major":7,"minor":0,"bytes":1073741824,"uuid":"00000000-0000-0000-0000-000000000000"}]
}
```

`POST /storage/event` 复用事件结构。事件顺序为 prepare → launching → starting(pid/starttime) → ready(devices) → stopped。每次先写原生日志、fsync、rename、fsync 目录，再发送通知。重复同序号和同内容允许；不同内容冲突；小序号不能覆盖大序号；stopped 后不能复活。

launching 在 spawn 之前持久化。starting 在 Firecracker spawn 后、配置或恢复 VM 之前持久化并获取继续启动授权。ready 必须经过 guest 容量/UUID/ext4 挂载/只读检查。stopped 必须在 VMM 退出后写入；缺少 PID 的 launching 结果未知，不产生停止证明。

Node sidecar 的 mTLS `/v1/evidence` 接受 SSF 原生 HTTPS EvidenceVerifier 的 `{sandbox,action,evidence}`；action 为 ready 或 detached。重新验证具体 native instance、boot、PID namespace、进程 starttime、设备集合以及 VMM 的实际块设备 FD。ready 时 readonly 设备 FD 必须 O_RDONLY。验证成功 HTTP 204，无法确认则 409/403。Hook 200、envd health、HTTP 404、租约过期都不是证据。

原生日志由可信 AgentENV 写，适配器不能根据自己的请求记录凭空生成 guest 成功。停止记录首次通知丢失时，节点扫描可信目录按 ticket 恢复映射。若同一 ticket 对应多个 native instance，保留引用并报冲突。
