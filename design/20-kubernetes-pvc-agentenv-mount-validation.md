# Kubernetes PVC → AgentENV `/data`：两步验证

本文验证一个场景：创建一个数据卷，沙箱 A 写入文件，删除 A 后让沙箱 B 挂载同一 PVC，文件仍在。使用本次更新后的二进制和部署增量；旧镜像不会因为更新文档就获得直接 PVC 能力。实现及本机验证状态见 [22](22-volume-preparation-implementation.md)。本文中的集群操作尚未在用户环境执行。

## 开始前

管理员只需完成一次 [部署增量](22-volume-preparation-implementation.md#部署增量)：启用 `agentenv-block-ext4-v1` profile、Guard mutation/validation，以及更新 Scheduler 和节点插件。选用支持 Block 的专用 StorageClass，保持原厂商 provisioner/parameters。用户不需要创建初始化 Pod、运行 mkfs 或调用 `/volumes import`。

下面假设 namespace 为 `agentenv-system`，专用 StorageClass 为 `agentenv-block`。使用已经能通过原 AgentENV 冷启动接口启动、包含 envd 的镜像。管理机安装 `kubectl`、`curl`、`jq`，并通过原 `aenv auth` 配好 CLI 访问同一个 Gateway。

```bash
export AENV_URL='https://你的原Gateway地址'
export AENV_IMAGE='你的已验证可启动镜像'
export AENV_API_KEY='你的API密钥'
```

有私有 CA 时给 `curl` 配置 `CURL_CA_BUNDLE`，不要关闭 TLS 校验。下面使用固定测试名称；若已存在该名称，请换新名称，不删除未知 PVC。

## 第一步：通过 Kubernetes 创建 PVC

```bash
kubectl create -f - <<'YAML'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: ssf-notes
  namespace: agentenv-system
spec:
  storageClassName: agentenv-block
  volumeMode: Block
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
YAML
```

查看状态：

```bash
kubectl get pvc ssf-notes -n agentenv-system -o json |
  jq '{phase:.status.phase,pv:.spec.volumeName,preparation:.metadata.annotations["ssf.dev/preparation-state"]}'
```

- **Immediate**：即使没有沙箱，后台协调器也会准备卷，最终显示 `Bound` / `VolumeReady`。准备节点只是临时节点，完成后释放 attachment。
- **WFFC**：没有消费者时可以保持 `Pending` / `WaitingConsumer`。直接进行第二步，不要先等待 Bound，也不要创建初始化 Pod。原 Scheduler 为沙箱选点后完成绑定和准备。

`Bound` 只表示 PVC/PV 绑定完成，不等于 guest 已挂载。准备状态尚未显示时等待后台观察；`NeedsRecovery` 或 `Failed` 时保留现场，查看第 6 节。

## 第二步：调用原创建接口引用 PVC

```bash
jq -n --arg image "$AENV_IMAGE" '{
  image:$image,
  timeout:3600,
  volumeMounts:[{
    pvc:{namespace:"agentenv-system",name:"ssf-notes"},
    path:"/data"
  }]
}' > /tmp/ssf-notes-create.json

curl --fail-with-body -sS -D /tmp/ssf-notes-headers \
  -H "X-API-Key: $AENV_API_KEY" -H 'Content-Type: application/json' \
  --data-binary @/tmp/ssf-notes-create.json \
  "$AENV_URL/sandboxes-cold" > /tmp/ssf-notes-a.json

cat /tmp/ssf-notes-headers
export SANDBOX_A="$(jq -er '.sandboxID' /tmp/ssf-notes-a.json)"
aenv exec "$SANDBOX_A" sh -c 'printf "hello from PVC\n" > /data/notes.txt; sync; cat /data/notes.txt'
```

预期 HTTP **201**，输出 `hello from PVC`。成功响应在 guest 设备、UUID、挂载路径及读写属性验证并提交后返回。

`volumeMounts` 每项必须恰好有一个 `name` 或 `pvc`。原来 `{"name":"原卷别名","path":"/data"}` 的形式仍保留。PVC namespace 必须在可信主体授权范围内；省略时只使用部署明确配置的默认 namespace。用户不需要提供 UID，框架首次解析后固定 UID。

## 验证数据持久化

```bash
curl --fail-with-body -sS -X DELETE -H "X-API-Key: $AENV_API_KEY" \
  "$AENV_URL/sandboxes/$SANDBOX_A"

kubectl get pvc ssf-notes -n agentenv-system

curl --fail-with-body -sS -H "X-API-Key: $AENV_API_KEY" \
  -H 'Content-Type: application/json' --data-binary @/tmp/ssf-notes-create.json \
  "$AENV_URL/sandboxes-cold" > /tmp/ssf-notes-b.json

export SANDBOX_B="$(jq -er '.sandboxID' /tmp/ssf-notes-b.json)"
aenv exec "$SANDBOX_B" sh -c 'cat /data/notes.txt'
```

预期删除返回 **204**，PVC 仍存在；B 创建返回 **201**，仍读到 `hello from PVC`。若创建返回 writer 冲突，先确认 A 的持久删除流程已经完成，不绕过使用权。网络超时导致创建结果未知时，先通过原 list/get 和持久请求记录定位实例，不盲目重复创建。

## 自动执行同一个场景

从 `sandboxVolume/SSF` 运行以下命令，工具会创建独立测试 PVC，并记录每个实例、PVC UID、PV UID 和逐项结果。不要在上面手工示例仍占用该卷时用 `--pvc ssf-notes` 再跑工具。

```bash
python3 scripts/native_pvc_e2e.py --execute \
  --url "$AENV_URL" --image "$AENV_IMAGE" \
  --namespace agentenv-system --storage-class agentenv-block \
  --driver-kind vendor --output validation/pvc-vendor
```

用另一个 WFFC 专用 StorageClass 再运行一次，输出到独立目录。fixture Driver 使用 `--driver-kind fixture`，不能将其结果当厂商 Driver 结果。

增加 `--lifecycle` 会继续检查 pause/hold/resume、v2 connect 自动恢复、快照/Fork 独立性、模板省略继承、空数组清除和显式 PVC 替换。快照/Fork 还需要相同 Driver 的唯一默认、Retain VolumeSnapshotClass 及原生 snapshot controller。

工具默认保留现场。先用 `native_cleanup.py` 查看并清理报告中记录的原生实例，再运行：

```bash
go run ./tools/cleanup-pvc -report validation/pvc-vendor/report.json
# 确认清单后，由操作者指定测试 kubeconfig 执行：
go run ./tools/cleanup-pvc -report validation/pvc-vendor/report.json \
  -kubeconfig /你的测试kubeconfig -execute
```

PVC 清理使用 UID 前置条件，只删除工具创建的 PVC；不会删除同名替代对象、移除 finalizer 或直接删除后端。Retain PV 后续由存储管理员按原 Kubernetes 流程处理。

## 定位失败

```bash
kubectl get pvc ssf-notes -n agentenv-system -o yaml
kubectl describe pvc ssf-notes -n agentenv-system
kubectl logs -n agentenv-system deployment/agentenv-scheduler --all-containers --tail=200
```

节点日志按实际 DaemonSet Pod 名查看。不要把含凭据的完整配置加入共享证据包。

| 现象 | 首先检查 |
| --- | --- |
| PVC 没有 profile/finalizer | mutating webhook 是否启用、namespace 是否匹配、专用 SC 注解是否正确 |
| Immediate 长期 Pending | 原 CSI provisioner、容量/配额、拓扑和 Kubernetes 事件 |
| WFFC 在创建前 Pending | 正常；调用原创建接口触发原 Scheduler 联合选点 |
| Bound 但未 VolumeReady | Scheduler 准备协调器、健康节点、mTLS、Node loopback credential、CSI Node UDS |
| Failed | profile 不兼容、只读空盘、其他 FS、未知非零内容、恢复源变化；不得强行格式化 |
| NeedsRecovery | 原 operation 的 WAL、设备锁和真实结果；不得换 operation 绕过未知格式化 |
| DELETE 后 Terminating | 准备/运行/暂停/checkpoint 引用仍在，正常 finalizer 排空；不强制移除 |
| 同名重建后旧请求拒绝 | UID 固定保护生效；创建新的请求，不能让旧恢复/模板静默重绑 |

本手册的通过标准仅是报告中实际执行成功的项目。Linux 设备故障、KVM 生命周期和真实厂商 CSI 的完整验收矩阵见 [22](22-volume-preparation-implementation.md)。
