# Kubernetes 管理卷、Substrate 模式直接挂载的沙箱存储框架

**卷管理保留 Kubernetes 原生体系：StorageClass、PVC、PV、供卷、绑定、快照、克隆、扩容和回收。沙箱消费参考 Substrate：创建请求直接驱动 Controller Attach、节点 Stage/Publish 和运行时接入，不创建逐沙箱消费者 Pod，不等待 kubelet 卷调谐。**

框架暂称 SSF（Sandbox Storage Framework）。本次重新设计将双供卷入口收敛为唯一 Kubernetes 管理入口；Substrate 的直接 CreateVolume/DeleteVolume、Actor 专属卷模型和整套控制面不引入。消费协调模块默认嵌入项目现有控制面，无须额外部署 `ssf-controller` Pod。

更新：2026 年 10 月 19 日。公共框架代码位于 [SSF](../SSF/README.md)，已进入实现与验证阶段。实际测试状态见 [实现验证报告](10-implementation-validation.md)，不能将本机单测视为 Linux/VM 性能实测。

## 阅读顺序

| 文档 | 重点 |
| --- | --- |
| [现有项目分析](01-repository-analysis.md) | CubeSandbox v0.7.3、AgentENV v0.2.3 的代码及数据语义；历史源码分析保持不变 |
| [选型与职责边界](02-csi-decision.md) | Kubernetes 管理、Substrate 消费的分工，以及此次收敛的决策 |
| [整体架构与接口](03-framework-design.md) | 模块、请求、状态、执行时序、运行时和故障恢复 |
| [项目接入与验证](04-integration-and-validation.md) | Cube/AgentENV 改造、迁移、PoC 和性能验收 |
| [Kubernetes 生命周期集成](05-kubernetes-integration.md) | Immediate/WFFC、PVC/PV 解析、唯一管理权、删除与原生回收 |
| [Substrate 实现分析](06-substrate-storage-analysis.md) | 当前 Substrate 的原始实现证据；分析文档保持不变 |
| [Substrate 代码复用与实现设计](07-substrate-inspired-implementation.md) | 可复制、需修改、不引入的源码，以及公共包边界 |
| [完整部署与模块归属](08-full-deployment-architecture.md) | 全部相关 Pod、容器、进程，以及原生/新增/项目定制的区分 |
| [软件实施记录](09-implementation-log.md) | 实施顺序、关键修正与环境限制 |
| [实现验证报告](10-implementation-validation.md) | 已执行检查、跳过项及复现命令 |
| [实现覆盖矩阵](11-implementation-coverage.md) | 设计到公共软件模块、测试与项目扩展的映射 |
| [AgentENV 独立适配设计与实现](12-agentenvadapter-design-and-implementation.md) | 适配代码边界、通用原生补丁、部署、真实证据与验收限制 |
| [AgentENV 端到端安装部署手册](13-agentenvadapter-installation-and-deployment.md) | 环境、构建、证书、数据库、CSI/Guard、模板、读写验收、恢复与生产替换 |
| [AgentENV 真实环境测试方案](14-agentenvadapter-real-environment-test-plan.md) | 场景自动化、逐步人工测试、故障注入、设备证据、性能与清理 |
| [用户指定 K8S CSI 架构参考](assets/K8S-CSI架构分析.md) | 架构与图形样式参考，原文不修改 |

## 架构图

![关键模块](figures/ssf-key-modules.svg)

[可编辑 Mermaid](figures/ssf-key-modules.mmd) · [PNG](figures/ssf-key-modules.png) · [部署概览](figures/ssf-deployment.svg) · [调用关系](figures/ssf-module-calls.svg)

![完整部署与子模块](figures/ssf-full-deployment.svg)

[放大 SVG](figures/ssf-full-deployment.svg) · [PNG](figures/ssf-full-deployment.png) · [Mermaid](figures/ssf-full-deployment.mmd) · [部署清单](08-full-deployment-architecture.md)

图中灰蓝 K 表示原生 Kubernetes/官方 CSI，青蓝 N 表示新增公共实现，棕色 P/D 分别表示项目定制和存储 Driver，绿色 B 为后端，灰色虚线框为条件部署。公共模块可以嵌入项目进程；蓝色框不等于新增 Pod。CSI Controller 与 Node 由调用方分别驱动，业务 I/O 不经管理 RPC 转发。

## 最终职责

| 能力 | 唯一负责方 |
| --- | --- |
| 卷定义、StorageClass、PVC/PV、配额及供卷 | Kubernetes API、PV Controller、external-provisioner、厂商 Driver |
| 后端卷删除 | Kubernetes 原生回收流程；沙箱侧先排空和授权，不直接 DeleteVolume |
| 后端 snapshot/clone、Controller 扩容 | 按需启用的原生快照/供卷/扩容组件；Runtime 另做一致性与 guest 配合 |
| 独立沙箱 WFFC 选点 | 项目调度器 + 公共 DelayedBinding；原生组件继续创建卷并完成绑定 |
| 沙箱使用授权、持久步骤、Attach/Detach | 嵌入现有项目控制面的公共消费协调模块 |
| NodeStage/Publish 及逆操作 | 公共 SSF Node Agent，经本机 UDS 调用厂商 CSI Node |
| 文件共享、虚拟磁盘、guest 就绪 | 各项目 Runtime Adapter，参考 Substrate 分层接入 |
| 使用引用、删除屏障、未知结果恢复 | 全局消费账本、独立准入 Guard、节点 WAL；不建立第二套卷生命周期 |

对外仍可提供项目原有 Volume API，但其 Create/Delete/Snapshot/Clone/Expand 实现必须转换为 Kubernetes 资源操作。`volumeUID` 是稳定的使用索引，PVC/PV 是管理权威，CSI handle 来自真实 PV；不允许通过索引单独创建或删除后端卷。

## 创建沙箱的路径

```text
CreateSandbox(volumes = PVC 引用或已登记 volumeUID)
 → 解析身份/权限/能力；联合计算与存储选点
 → 必要时 WFFC 选点预留、原生供卷/绑定，确认真实 Bound
 → 登记使用引用、writer/node pin；直接 EnsureAttachment
 → 现有创建 RPC 携带受约束 NodeVolumePlan
 → Node Agent 同步 Stage/Publish
 → Runtime Adapter 接入目录或块设备，确认 GuestReady
```

静态 PV+预绑定 PVC 或提前完成的 Immediate PVC 可将供卷移出启动路径；不代表提前固定或 Attach 到 Worker。WFFC 同属首期正式能力：动态卷在首次消费时供卷，静态卷在首次消费时选卷/绑定；绑定后共用直接消费路径。

正常请求主动推进步骤，后台执行相同处理器完成恢复、补偿和释放。控制面仍需要使用记录和排他保护，但不通过创建 Binding CRD 后等待调谐来触发普通沙箱挂载。watch 用于跟踪原生资源变化和 WFFC 结果，不是已就绪卷每次挂载的触发器。

## 性能与兼容范围

优化重点为提前供卷、常驻 Driver/长连接、预先完成能力发现、兼容的 attachment/stage 复用、多卷批量及并行准备。后端真实 Attach、Publish、VMM 与 guest 接入和权威使用授权全部计入端到端延迟，不承诺所有存储都有相同冷启动毫秒值。

Kubernetes 协议与资源兼容不等于自动具备独立 VM 的引用保护。生产使用需完成专用资源范围、默认 Retain、Guard、事务账本和回收屏障；同一后端资源不由 kubelet、external-attacher 与 SSF 同时管理相同消费操作。

CubeSandbox 保持外部卷 Reference/Share；AgentENV 保持可写卷 Capture/Clone、只读共享与层发布约束。存储差异由 Driver 表达，VMM/guest 差异由 Runtime Adapter 表达。纯宿主运行的沙箱节点也可以调用 Kubernetes 管理面；完全没有 Kubernetes API 的独立供卷不在本方案范围。
