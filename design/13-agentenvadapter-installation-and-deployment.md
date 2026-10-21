# AgentENV 原生插件安装与部署

> 2026-10-21 自动卷准备增量：实现、部署、验证结果及未完成边界统一见 [22](22-volume-preparation-implementation.md)；直接 PVC 两步场景见 [20](20-kubernetes-pvc-agentenv-mount-validation.md)。本文件原生命周期说明继续适用，旧未启用 profile 的卷策略不代表新 profile 规则。

代码入口与职责见 [12](12-agentenvadapter-design-and-implementation.md)，验收见 [14](14-agentenvadapter-real-environment-test-plan.md)。本文为部署材料，不代表已部署。当前只在本机验证渲染、凭据和构建；Linux/KVM/K8s/systemd/真实 CSI 未执行。

![部署边界](figures/agentenvadapter-deployment.svg)

## 集群部署

使用 `AgentENV/deploy/k8s/overlays/ssf` 增量叠加原 Kustomize base。保留原 Gateway/Scheduler Deployment、node DaemonSet、headless Service、心跳、P2P、探针及优雅退出；增加 Scheduler 内嵌消费库、node 内 NodePlugin/SSF Node sidecar、RBAC、WAL 和挂载传播。不部署独立沙箱控制面，不配置静态 workers/templates。

前提：Linux/KVM 原 AgentENV 可用；Kubernetes 支持 native sidecar `initContainers[].restartPolicy: Always`；安装对应真实 CSI Controller/Node、snapshot CRD/controller/sidecar、SSF Guard 和管理网关，提供 PostgreSQL。首规格要求 Block/ext4、匹配 CSINode 和可访问 CSI Node UDS；SnapshotClass 和 SC 按 SSF 管理策略 Retain。扩容还要求 SC allowVolumeExpansion 与对应 Controller/Node 能力。

从 `sandboxVolume/SSF` 执行以下本地准备步骤：

```sh
python3 scripts/native_credentials.py --out /tmp/ssf-native-credentials --namespace agentenv-system --native-api-key-file /secure/AENV_API_KEY --database-url-file /secure/ledger-url
python3 scripts/render_agentenv_native.py --kustomize /path/to/kustomize --namespace agentenv-system --api-key-file /tmp/ssf-native-credentials/AENV_API_KEY --driver example.csi.driver --controller-endpoint csi-gateway.ssf-system.svc:9443 --output /tmp/agentenv-native.yaml
go run ./tools/check-native-deploy -file /tmp/agentenv-native.yaml -namespace agentenv-system
```

生成器拒绝覆盖已有目录，目录 0700、私钥与 Secret 清单 0600；独立 CA、node/control/ssf-node 证书、URI 身份、DNS/localhost/IP SAN、Ed25519 计划密钥和四组 HMAC/token 分离。已有原生 API Key 应复用；DB URL 不打印。输出既有 Kubernetes Secret List，也有每个 Secret 的私有文件目录，可用于 standalone。示例本地 CA 用于测试，生产按现有 PKI 轮换。

构建不修改原普通 Go 依赖：

```sh
cd ../AgentENV/services
go build ./scheduler/cmd ./gateway/cmd
go build -modfile=go.ssf.mod -tags ssf ./scheduler/cmd ./gateway/cmd
cd ../..
docker build -f AgentENV/deploy/docker/Dockerfile.scheduler.ssf -t agentenv-scheduler:ssf .
docker build -f SSF/adapter/agentenvadapter/Dockerfile -t agentenv-storage:ssf SSF
```

原 AgentENV 与 SSF Node 镜像沿用各自构建入口；修改渲染后的镜像引用、SC/Driver、真实 namespace、Controller TLS 和 PostgreSQL Secret。操作员审核完整清单后自行 `kubectl --context CONTEXT apply -f ...`。本文实现过程不自动提交或部署。

节点 hook 绑定 Pod loopback `:9445`，使用独立 hook-token；NodePlugin `:9444` 与 SSF Node `:9443` 使用 mTLS。共享宿主 PID、/dev 与持久目录：`/var/lib/ssf`、`/var/lib/agentenvadapter`、`/var/lib/agentenv-external`、原 `/var/lib/aenv`。CSI/SSF Bidirectional、VM HostToContainer 的传播方式不可省略。SSF Node 先启动后退出；联合 drain 等待 VMM 停止与存储释放，期限到达保留 WAL，不强制清理未知设备。

## standalone

使用 [standalone 材料](../SSF/adapter/agentenvadapter/deploy/standalone/README.md)。保留原 `agentenv.service` 二进制、ExecStart 和公开 API；仅加环境 drop-in、新增存储插件与 SSF Node 服务。本地插件承载同一 NativeControl/Coordinator 并固定本机 placement。仍需要 Kubernetes 管理卷，宿主必须是配置中的真实 K8s Node；不是无 Kubernetes 的存储模式。

分别配置本地 `node.json`、`control.json`、`ssf-node.json`、kubeconfig 和环境文件。按生成的私有文件映射安装证书/密钥，不复用不同 URI 身份。设置 `AENV_STORAGE_PROVIDER_STANDALONE=true`，不同时配置 Scheduler 控制端点。systemd 不使用 PrivateMounts/PrivateDevices 隔离共享路径。启停和 drain 详见该目录 README。

## 验证与回滚

部署前执行 `native_preflight.py --mode cluster|standalone`、离线渲染和只读证据采集。部署后先执行无 SSF 卷兼容测试，再进行专用测试卷 E2E。停用 provider 前先排空所有运行、暂停、模板及持有引用；确认账本与 WAL 无需服务的资源后移除 overlay/drop-in。不要直接删除 WAL、Guard、账本或已挂载设备。旧原型无存量，已删除旧独立 API/部署/客户端，不提供迁移兼容层。

设备槽位在捕获模板时固定。overlay 和 standalone 样例额外预留 1 个只读槽位（`AENV_STORAGE_PROVIDER_READONLY_SLOTS=1`），不占用原 legacy 可写卷额度；需要更多只读盘时在生成模板前调整配置。显式替换还需容纳保留旧盘与新盘；超过原 Firecracker/AgentENV 盘位上限时明确失败，不替换仍被内存引用的盘。
