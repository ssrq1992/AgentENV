# 部署与隔离

首次部署请按 [端到端安装部署手册](../../../../design/13-agentenvadapter-installation-and-deployment.md) 执行。完整手册补齐 PostgreSQL、Guard、Gateway、测试 CSI、模板构建和验收流程；[安装覆盖渲染器](installation/render.py) 只生成单 Worker 测试清单，不直接操作集群。本页保留组件部署要点。

先阅读 [独立设计文档](../../../../design/12-agentenvadapter-design-and-implementation.md) 的限制及故障恢复规则。以下命令从 SSF 根目录运行，仅用于已授权的专用测试集群。`example.rendered.json` 是可审查示例，需要替换 Worker、Driver endpoint、镜像和模板 ID；脚本不会直接操作集群。

## 前置设施

1. Linux 6.8+ amd64 Worker，KVM、ublk、loop 和 mount propagation 可用；保留 CPU/内存给内核、CSI、AgentENV 及 SSF，`capacity` 只填写沙箱可用余量。禁止普通 AgentENV Gateway/创建入口调度进这个池。
2. 安装厂商 CSI Controller/Node、StorageClass、CSINode；或在隔离单节点 Linux 集群运行 `scripts/render_fixture.py` 生成的测试 Driver。该本地 loop Driver 不证明真实远端存储性能或多节点可迁移性。
3. 部署现有 SSF PostgreSQL、Guard 与 webhook，应用 `deploy/consumption-rbac.yaml`、`deploy/base.yaml` 中的角色/Guard 资源。**不要在同一 Worker 再运行 base.yaml 的通用 ssf-node DaemonSet**；适配渲染器生成每节点专属 SSF Node 和 EvidenceEndpoint。`ssf-node` ClusterRole 必须存在。
4. CSI Controller 仅提供 UDS 时，把 `ssf-csi-gateway` 与 Driver Controller 放同一 Pod，挂相同 socket；其 `AllowedPeers=["spiffe://ssf/agentenv/control"]`，共享同一账本。Gateway `DriverName/DriverEndpoint` 按 `deploy/gateway.example.json` 配置。测试 fixture 同 Pod 内的 Gateway 可使用其 `/csi/csi.sock`，Service 指向该唯一 Driver 实例；不要把本地 fixture 的 Controller Service 随机分发到不同 Worker。
5. Worker、原生 CSI Node 与 SSF Node 必须看到一致的 `/var/lib/ssf` 和块设备；CSI/SSF 使用 Bidirectional，AgentENV sidecar 与 VMM 使用 HostToContainer。保留这三个 HostPath 的内容，不能删 WAL 来“修复”挂载。

## 配置、证书、迁移

```sh
cp adapter/agentenvadapter/config/control.example.json /tmp/agentenv-control.json
# 编辑 workers 的实际 Kubernetes Node 名称、容量、Driver TLS endpoint；按模板真实 manifest 填 catalog。
python3 adapter/agentenvadapter/scripts/render.py --control /tmp/agentenv-control.json > /tmp/agentenvadapter.json
kubectl --context YOUR_TEST_CONTEXT apply --dry-run=server -f /tmp/agentenvadapter.json
```

镜像构建：`make -C adapter/agentenvadapter image`、`bash adapter/agentenvadapter/scripts/build-agentenv.sh`，SSF 公共镜像仍用根目录 Dockerfile。AgentENV 补丁已直接放进源码，不要再对当前源码应用一次补丁。

生产证书由既有 PKI 签发。每份证书只有一个 URI SAN，身份和 DNS 必须匹配：

| 证书 / Secret | URI SAN | 服务 DNS SAN |
| --- | --- | --- |
| agentenv-control-tls | spiffe://ssf/agentenv/control | agentenv-control.agentenv-ssf.svc |
| aenv-worker-1-tls | spiffe://ssf/agentenv/node/worker-1 | aenv-worker-1.agentenv-ssf.svc |
| ssf-aenv-worker-1-tls | spiffe://ssf/agentenv/ssf-node/worker-1 | ssf-aenv-worker-1.agentenv-ssf.svc |
| 租户客户端 | spiffe://ssf/agentenv/user/demo | 无服务端 DNS 要求 |

可用 `scripts/dev-certs.py --control /tmp/agentenv-control.json --out /tmp/aenv-test-pki` 生成仅 7 天有效的测试 CA、TLS 和签名材料。此脚本要求支持 Ed25519 的 OpenSSL。不要将生成的私钥提交到仓库。Guard/Gateway 必须使用同一 CA 或正确配置互信；其证书仍按 SSF 的配置流程签发。`agentenv-ledger` Secret 的 `url` 为共享 PostgreSQL DSN；不是第二份消费账本。`agentenv-native-key` 仅挂入原生 AgentENV 和节点适配器。

先创建 Namespace、Secrets、ConfigMap、SA/RBAC，再运行渲染结果的 `agentenv-migrate` Job，确认 Complete 后启用控制面及节点 DaemonSet。控制面在表不存在时不会就绪。渲染结果为 Kubernetes List，可使用标准 JSON 工具按 kind 分组应用。Node 服务启动需先能读取 Node UID；SSF Node 不依赖每沙箱 Pod。

NetworkPolicy 需要集群 CNI 支持。节点 Pod 使用 `shareProcessNamespace=true`，不使用 hostPID。原生 API `127.0.0.1:8000`、Hook `127.0.0.1:9445` 均不通过 Service 暴露。保留管理者专属 RBAC，避免非授权人员 exec 进入特权 Pod。外部用户只访问控制面 mTLS API。

## 模板

在独立模板构建节点使用同样的 `[volume] max_volume_count=8` 和 `[custom_extension] storage_readonly_slots=2`，但 `storage_required=false`，且不要配置适配器 Hook。使用 AgentENV 原有模板构建接口生成不带外部卷的模板。新 manifest 会保存 `volumeDriveSlots`、`volumeReadonlySlots`；不要手工把老 manifest 改成支持只读槽位。

把确定的不可变 snapshot/template ID、CPU、memory_mib、可用 RW/RO 槽位录入控制配置 `templates`。例如：

```json
{"templates":{"IMMUTABLE_ID":{"id":"IMMUTABLE_ID","resources":{"cpu":2,"memory_mib":512},"rw_slots":6,"ro_slots":2}}}
```

控制入口先校验 catalog，原生 Provider 再核对实际模板资源，实际设备规划再次检查槽位。不支持动态别名解析；模板旧版本缺少只读槽位时拒绝只读挂载。

## 首次使用和诊断

客户端需要本租户证书。示例：

```sh
python3 adapter/agentenvadapter/examples/client.py --url https://CONTROL:9444 --ca CA --cert CERT --key KEY volume adapter/agentenvadapter/examples/create-volume.json
python3 adapter/agentenvadapter/examples/client.py --url https://CONTROL:9444 --ca CA --cert CERT --key KEY create adapter/agentenvadapter/examples/create-cold.json
# 用返回的 id 轮询，Ready 后：
python3 adapter/agentenvadapter/examples/client.py --url https://CONTROL:9444 --ca CA --cert CERT --key KEY exec OPERATION_ID 'echo hello > /data/hello; sync; cat /data/hello'
```

`/readyz`、`/livez`、`/metrics` 使用独立健康端口 9091；不返回密钥或计划。API `reason` 和操作 ID 用于定位。节点 bbolt、原生 `/var/lib/agentenv-external/<instance>.json`、SSF WAL 与数据库要一起保留。读取这些日志需要运维权限。

正常停止先 best-effort guest sync/unmount，再等 VMM 确认退出，最后 Release/Detach。异常中断可能留下待恢复的 ext4 日志；只读挂载遇到脏文件系统会拒绝，不强制跳过日志或重格式化。

只重启某个进程时可恢复日志。**同一 Boot ID 下替换整个 Pod 会改变 PID namespace**，新 sidecar 不能凭“看不见旧 PID”认定旧进程已退出；此时保留引用。先排空再滚动 Pod；无法确认的旧实例需要运维执行可证明的节点 fencing（例如受控重启，且 Kubernetes Node UID 不变）。没有“强制解锁成功”接口。
