# SSF + AgentENV 最简操作：创建卷、挂载、删除沙箱

适用：环境已经按 [安装手册](13-agentenvadapter-installation-and-deployment.md) 部署完成。本次只做 **创建 PVC → 创建沙箱并挂载到 `/data` → 读写 → 删除沙箱**。

采用动态供卷：通过适配 API 创建原生 PVC，Kubernetes/CSI 自动创建 PV；创建沙箱时引用 PVC。以下调用的入口是 **agentenvadapter control**。需要管理机已有 `kubectl`、`python3`、`jq` 和租户客户端证书。

## 1. 连上服务，设置变量

**终端 A**：确认当前集群正确，开启端口转发，保持终端运行。

```bash
kubectl config current-context
kubectl -n agentenv-ssf port-forward service/agentenv-control 9444:9444
```

**终端 B**：先单独执行 `bash`，后续步骤都在这个 Bash 会话中执行。修改源码目录和证书目录；Namespace、StorageClass、镜像与实际环境不同时也修改。每一步报错就先停下检查。

```bash
cd /path/to/sandboxVolume/SSF

PKI='/absolute/path/to/pki'             # 内含 ca.crt、user.crt、user.key
NS='agentenv-demo'                     # 当前用户证书获授权的 Namespace
SC='ssf-block-immediate'               # 已接入 SSF 的动态 Block、Retain StorageClass
IMAGE='docker.io/library/ubuntu:24.04'  # Worker 能拉取的镜像
API='https://127.0.0.1:9444'
CTX=$(kubectl config current-context)

umask 077
RUN=$(mktemp -d "${TMPDIR:-/tmp}/ssf-quick.XXXXXX")
PVC="quick-$(date +%Y%m%d%H%M%S)-$RANDOM"

k() { kubectl --context "$CTX" "$@"; }
client() {
  python3 adapter/agentenvadapter/examples/client.py --url "$API" \
    --ca "$PKI/ca.crt" --cert "$PKI/user.crt" --key "$PKI/user.key" "$@"
}

k get storageclass "$SC"
printf '请求及结果目录：%s\n本次 PVC：%s\n' "$RUN" "$PVC"
```

确认终端 A、B 是同一集群。若已有可用的 HTTPS 适配入口，可省略终端 A，将 `API` 换成实际地址。

## 2. 创建一个 1 GiB PVC

```bash
jq -n --arg ns "$NS" --arg name "$PVC" --arg sc "$SC" \
  '{namespace:$ns,name:$name,storage_class:$sc,bytes:1073741824,initialize_ext4:true}' \
  > "$RUN/volume-request.json"

client volume "$RUN/volume-request.json" > "$RUN/volume-response.json"
PVC_UID=$(jq -er '.metadata.uid' "$RUN/volume-response.json")
k -n "$NS" get pvc "$PVC"
```

**预期：** PVC 创建成功。`Immediate` 类稍后应为 `Bound`；若使用 WFFC 类，此时 `Pending` 正常，直接进入下一步。

`initialize_ext4:true` 授权首次挂载时初始化这块**本次新建的空卷**。本流程通过适配 API 登记授权；仅 `kubectl apply` 一个空 PVC 不会自动获得格式化授权。

## 3. 创建沙箱，挂载 PVC 到 `/data`

```bash
jq -n --arg id "$PVC" --arg ns "$NS" --arg pvc "$PVC" \
  --arg uid "$PVC_UID" --arg image "$IMAGE" \
  '{request_id:$id,kind:"cold",
    launch:{image:$image,cpuCount:2,memoryMB:512,timeout:1800},
    mounts:[{namespace:$ns,pvc:$pvc,uid:$uid,id:"data",path:"/data",readonly:false}]}' \
  > "$RUN/sandbox-request.json"

client create "$RUN/sandbox-request.json" > "$RUN/sandbox-response.json"
OP=$(jq -er '.id' "$RUN/sandbox-response.json")
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
```

重复下面这一条，直到 `state` 为 **`Ready`**，再进入下一步：

```bash
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
```

首次镜像准备和空卷初始化可能较慢。`OP` 是适配 operation ID，后面的查询、执行和删除都使用它。请求若超时，重试同一个 `sandbox-request.json`，不要重新生成请求 ID。

## 4. 在沙箱内验证挂载和读写

```bash
client exec "$OP" 'set -eu
grep " /data " /proc/mounts
printf "hello-ssf\n" > /data/hello.txt
sync
cat /data/hello.txt'

PV=$(k -n "$NS" get pvc "$PVC" -o jsonpath='{.spec.volumeName}')
k -n "$NS" get pvc "$PVC"
k get pv "$PV"
```

**预期：** guest 输出 `/data` 的 `ext4`、`rw` 挂载记录以及 `hello-ssf`；PVC 和对应 PV 都为 `Bound`。这一步验证的是 SSF 数据卷。

## 5. 删除沙箱，确认卷保留

```bash
client delete "$OP" > "$RUN/delete-response.json"
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
```

重复查询，直到 `state` 为 **`Released`**：

```bash
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
```

然后确认 PVC/PV 仍在，PVC 身份不变：

```bash
k -n "$NS" get pvc "$PVC"
k get pv "$PV"
k -n "$NS" get pvc "$PVC" -o json | jq -e --arg uid "$PVC_UID" '.metadata.uid==$uid'
```

**预期：** `Released`、PVC/PV 仍为 `Bound`、最后输出 `true`。本次关键流程完成，PVC 和数据保留。DELETE 返回成功只是接受删除请求，需要等到 `Released` 才算释放完成。

## 可选：重建一次，确认数据还在

沿用同一 PVC，只换创建请求 ID：

```bash
jq --arg id "$PVC-reopen" '.request_id=$id' "$RUN/sandbox-request.json" \
  > "$RUN/reopen-request.json"
client create "$RUN/reopen-request.json" > "$RUN/reopen-response.json"
OP=$(jq -er '.id' "$RUN/reopen-response.json")
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
```

按第 3 步重复查询到 `Ready` 后执行：

```bash
client exec "$OP" 'cat /data/hello.txt'
client delete "$OP"
```

应读到 `hello-ssf`。再按第 5 步查询到 `Released`，结束测试。

## 如果某一步失败

先停在失败步骤，查看以下结果；保留 `$RUN` 中的原请求和响应，用原请求重试。

```bash
client inspect "$OP" | jq '{id,state,reason,sandbox_id}'
k -n "$NS" describe pvc "$PVC"
k -n agentenv-ssf logs deployment/agentenv-control --tail=80
```

创建尚未返回 `OP` 时，先看客户端报错，并重试原创建文件。TLS 报错检查入口与证书；PVC 长时间 Pending 查看 PVC Events；沙箱未 Ready 查看 `reason`。详细排查见 [完整测试方案](14-agentenvadapter-real-environment-test-plan.md)。

本文已核对当前 API 和客户端命令；未连接你的环境执行。
