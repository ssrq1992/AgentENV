#!/usr/bin/env python3
"""Fixed-layout SVG companion to the editable Mermaid sources."""
from pathlib import Path
from html import escape
R=Path(__file__).resolve().parents[4]/'design/figures'
colors={'K':('#24364a','#617d9b'),'N':('#244351','#739bad'),'A':('#44392c','#aa9066'),'D':('#3d3328','#937b56'),'B':('#253a33','#668b7d')}
def svg(title,groups,name):
 out=['<svg xmlns="http://www.w3.org/2000/svg" width="1480" height="1300" viewBox="0 0 1480 1300">','<rect width="1480" height="1300" fill="#0f1822"/>','<g font-family="PingFang SC, Microsoft YaHei, sans-serif" fill="#c4d0dc">',f'<text x="32" y="48" font-size="30">{escape(title)}</text>','<text x="32" y="84" font-size="18">K 原生 Kubernetes　N 公共 SSF　A 项目适配　P 原生 AgentENV　E 通用扩展　D 厂商 Driver　B 持久存储</text>']
 for y,h,label,boxes in groups:
  out.append(f'<rect x="24" y="{y}" width="1432" height="{h}" rx="8" fill="#111c27" stroke="#64798d"/>')
  out.append(f'<text x="44" y="{y+32}" font-size="23">{escape(label)}</text>')
  width=(1392-20*(len(boxes)-1))/len(boxes)
  for i,(origin,lines) in enumerate(boxes):
   x=44+i*(width+20);fill,stroke=colors[origin];bh=h-76
   assert len(lines)*29+18<bh,(name,lines,bh)
   out.append(f'<rect x="{x}" y="{y+52}" width="{width}" height="{bh}" rx="6" fill="{fill}" stroke="{stroke}"/>')
   for j,line in enumerate(lines):out.append(f'<text x="{x+15}" y="{y+82+j*29}" font-size="19">{escape(line)}</text>')
 out+=['<text x="32" y="1268" font-size="18">部署边界与模块归属图；完整控制调用、CSI RPC 与数据路径见同名 Mermaid / lifecycle 图。</text>','</g></svg>']
 (R/(name+'.svg')).write_text('\n'.join(out))
svg('AgentENV × SSF · 模块与权威边界',[
 (112,245,'① Kubernetes 负责卷管理', [('K',['K：StorageClass / PVC / PV','PV Controller：原生绑定','官方 CSI sidecars：原生生命周期']),('D',['D：Controller / Identity','Create / Delete / Expand / Snapshot','通过原生组件进入后端']),('N',['N：独立 SSF Guard','受管 PVC / PV / Pod 准入','排空与回收许可'])]),
 (377,260,'② Adapter control 内嵌公共 Consumption', [('A',['A：mTLS API / 幂等 / Placement','不可复用 instance UID','固定 Node UID / Boot ID','租户 Namespace / 模板 catalog']),('N',['N：Resolver / WFFC / Acquire','Controller Attach / BuildPlan','Commit / Release / 恢复','仅消费接口，不供卷']),('B',['B：一个共享 PostgreSQL','ssf_*：消费权唯一权威','agentenv_adapter_*：项目映射','租约过期不代表 fencing'])]),
 (657,290,'③ Worker 直接执行，运行实例提供真实证据', [('A',['A：节点 sidecar / bbolt','签名计划 → SSF Prepare','受保护 ticket → AgentENV','E：设备 Provider / 生命周期日志','guest UUID / 容量 / RO 核验']),('N',['N：SSF Node / WAL','NodeStage / NodePublish','共享 holder / Unstage','HTTPS EvidenceVerifier','D：标准 CSI Node / UDS']),('A',['P：Firecracker 主机子进程','外部 virtio-block → guest ext4','原 OverlayBD / ublk 路径保留','API Ready 必须 guest 核验通过','停止 VMM 后才能释放设备'])]),
 (967,255,'④ 请求路径与业务 I/O', [('N',['创建：预留 → 绑定 → Attach','→ Prepare → guest ready → Commit','释放：停止确认 → Unpublish','→ 最后引用 Unstage / Detach']),('B',['业务 I/O：guest → virtio-block','→ host block device → 存储后端','不经过 control / Guard / Gateway','删除沙箱不会删除 PVC'])])
 ],'agentenvadapter-modules')
svg('AgentENV × SSF · 完整部署与进程边界',[
 (112,245,'① 集群管理组件 / 原生 CSI', [('K',['API Server / SC / PVC / PV','kube-controller-manager Pod','PV Controller / binder']),('D',['CSI Controller Pod','K：provisioner / resizer','K：attacher / snapshotter（按需）','D：厂商 Controller + Identity','N：条件 Gateway sidecar / UDS']),('N',['SSF Guard Deployment / Pod','Admission / 使用授权 / 排空','从共享账本校验保护状态'])]),
 (377,245,'② 适配控制面：独立 Deployment', [('A',['agentenv-control Pod / 进程','A：API / Placement / 元数据恢复','N：嵌入式 Consumption','mTLS → 固定 Worker Service']),('B',['外部持久 PostgreSQL','ssf_* + agentenv_adapter_*','不是两份消费账本','迁移：独立 agentenv-migrate Job'])]),
 (642,350,'③ 专用 Worker：Pod 内共享 PID namespace，SSF Node 另一个 Pod', [('A',['aenv-worker DaemonSet Pod','A：adapter node 容器','Prepare / Runtime / Evidence','P+E：AgentENV server 容器','loopback :8000 / Hook :9445','P：Firecracker 主机子进程','VM 内 guest / envd / ext4']),('N',['SSF Node DaemonSet Pod','gRPC :9443 / health :9090','N：Stage / Publish / shared refs','N：bbolt WAL / 恢复','HTTPS → sidecar Evidence','按 Worker 固定 Service','不创建逐沙箱 Pod']),('D',['CSI Node DaemonSet Pod','K：node-driver-registrar','D：Node + Identity','本机 UDS / CSI 标准 RPC','HostPath 共享真实 block target','不负责 guest 文件系统就绪'])]),
 (1012,210,'④ 宿主持久路径与挂载传播', [('B',['/var/lib/ssf：host publish + SSF WAL','/var/lib/agentenvadapter：适配 bbolt','/var/lib/agentenv-external：原生实例日志']),('B',['/var/lib/aenv：原生镜像与运行目录','/dev：块设备 / KVM / ublk','CSI+SSF Bidirectional；VM HostToContainer'])])
 ],'agentenvadapter-deployment')
