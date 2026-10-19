# Linux / 集群 / VM 故障验收

本文件是待执行的故障步骤及判据，不是执行结果。每项保存操作 JSON、节点 bbolt 诊断、AgentENV 原生事件、SSF WAL、共享账本与 CSI 日志。生产环境不执行这些注入。

逐场景的自动化和人工操作见 [真实环境测试方案](../../../../design/14-agentenvadapter-real-environment-test-plan.md)，工具见 [realenv](realenv/README.md)。当前 fixture 没有逐 RPC 故障配置开关；精准 NodePublish/Attach 响应丢失需额外 CSI 测试代理或厂商设施，未具备时保持 NOT RUN。

| 注入位置 | 操作 | 必须观察到 |
| --- | --- | --- |
| 控制面 POST 返回前断开客户端 | 用同 request_id 重试；再更换 mount 参数 | 原操作/instance 不变；不同参数 409 |
| 计算预留并发满额 | 运行 PostgreSQL 并发测试；超过 capacity 发请求 | 不超卖；过期未释放 reservation 不复用 |
| 多卷第二个 Prepare 失败 | 额外 CSI 测试代理/厂商设施对第二个 NodePublish 注入失败；普通 fixture 暂不提供此开关 | VM 未运行；第一个 Publish 补偿；最后一个 holder 才 Unstage/Detach |
| Attach 响应丢失 | 在 Gateway 前断链，原 Driver 执行成功 | 同一 volume/node 重试，无切点；不能根据 timeout 放弃引用 |
| WFFC 供卷结果未知 | selected-node 设置后暂停 provisioner，再删除创建操作 | 保留原选点；Bound 后完成补偿；不创建另一节点卷 |
| Prepare 后控制面崩溃 | 终止 control 进程，再启动相同 DB 配置 | 恢复同一 operation 与 instance |
| Create 发送后取消 | 并发调用 DELETE 与 native 创建 | revocation tombstone 阻止迟到计划；已获准启动的实例被停止；未确认前不 Released |
| Hook 回调丢失 | 阻断 loopback 9445，恢复后检查原生 fsynced journal | 通过可信日志发现 instance 并恢复；不会把 Hook 超时当作停止 |
| 原生 owner 崩溃 | 在共享 PID namespace 内终止 AgentENV server，保留 sidecar | 已登记 VMM 仍在时先用 pidfd 停止 orphan；确认退出后释放 |
| Node sidecar 崩溃 | 仅重启 adapter 进程，保留 Pod/PID namespace 和 WAL | 仍运行实例保留引用，guest 就绪证据重新核验 |
| SSF Node 崩溃 | 仅重启 ssf-node 进程，保留 WAL | Stage/Publish 不重复分配；释放顺序不变 |
| Pod 被替换 | 删除并重建整个节点 Pod，同 host Boot ID | PID namespace 变更导致拒绝释放；须受控 host fencing；不得假判旧 PID 消失 |
| 主机重启 | 同 Node UID、Boot ID 改变 | 旧 VM 已被 host reboot fencing；清理旧引用；不自动恢复内存 |
| Node 对象重建 | 同名字新 UID | 保留旧引用并诊断，不采用名字作为身份 |
| 旧事件重放 | 重放较小 sequence 或不同 native instance | 不回退 Ready/Stopped；身份冲突拒绝 |
| PVC 名字复用 | 删除旧 PVC 后以同名新 UID 重建 | 原操作仍固定旧 UID，不消费新卷 |
| 原生 Pod 引用受管 PVC | 创建 Pod 指向正在消费的 PVC | Guard 拒绝；未经 drain 不允许回收 |
| 正常删除与保留数据 | vm-e2e.py 的 cold/template RW/RO 四轮 | 删除沙箱保留 PVC，数据一致；RO 写失败 |
| 最后引用与孤儿检查 | 每次 Released 后查 native journal、/proc、SSF WAL、CSI 日志 | VMM 不再持有设备；Unpublish → 最后引用 Unstage → Detach；共享 holder 尚在时不拆共享挂载 |

静态 PV：先管理员创建已有 ext4 后端卷与受管 CSI PV，Retain；预绑定 PVC 或同类 WFFC PVC。已有卷的 `initialize_ext4` 不允许补录为 true。使用 `vm-e2e.py --existing-pvc STATIC_PVC --runs 1 --concurrency 1` 分别验证静态 Immediate 和静态 WFFC（仅用于可写测试卷，会写入 /data/retained）；动态运行 `vm-e2e.py --class ssf-block-immediate` / `--class ssf-block-wffc`。

真实测试命令：

```sh
bash adapter/agentenvadapter/tests/preflight.sh
bash adapter/agentenvadapter/tests/linux-integration.sh
# Linux 上另运行 AgentENV 原有 Rust 回归，关闭 external storage 扩展：
(cd ../AgentENV && cargo test --locked -p agentenv --lib)
python3 adapter/agentenvadapter/tests/vm-e2e.py \
  --url https://CONTROL:9444 --ca CA --cert CERT --key KEY \
  --image IMMUTABLE_ROOT_IMAGE --template IMMUTABLE_TEMPLATE_ID \
  --class ssf-block-immediate --runs 20 --concurrency 4 --report /tmp/real-vm-report.json
```

模板 RO 测试要求模板实际预留只读设备槽位。测试脚本不会删 PVC；需要在验收记录归档后显式排空回收。厂商 Driver 使用独立报告，不与测试 Driver 合并。`vm-e2e.py` 的性能包含客户端 50 ms 查询间隔；其 stage 字段为累计实际调用耗时，不能把 overall 减去这些值称为纯 Firecracker 启动时间。
