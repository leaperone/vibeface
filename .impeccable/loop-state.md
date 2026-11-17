# Vibeface 前端质量持续改进

## 目标与当前状态

目标是以 Impeccable critique 和 audit 的实际执行结果持续改进前端。保留 pinned Arc、现有功能和用户修改，每轮修改代码、测试并检查真实浏览器截图。停止门槛为 critique 至少适用总分的90%、audit没有P0/P1、没有明显影响体验的P2，连续两轮没有新有效问题。

基线已完成。评估者A/B在独立上下文和独立浏览器tab运行，A完成后B结果才进入综合。分数由实际执行的Impeccable参考评分产生，不调整分母以凑目标。

## 工作树与保护边界

- 本轮工作树 `/root/vibeface/.worktrees/feat-impeccable-quality-loop`，分支 `feat/impeccable-quality-loop`，起点 `d92822fd`。
- 主 checkout `/root/vibeface` 保持原状。上一轮 `audit/full-20261116` 的28个未提交文件原样保留，不混入本轮提交。
- 用户已授权本轮达标后合并主分支。发布、生产部署和修改凭证不属于此次任务。
- 只使用任务拥有的Node、浏览器profile和本机端口，不访问个人网络或真实AI。

## 已执行初始化

- 读取完整 Impeccable 4.9.1 SKILL.md，运行一次 `impeccable context`。
- 读取 Web/Mobile PRODUCT与DESIGN、Arc迁移约定、critique/audit与Operate参考。
- 读取项目e2e skill和现有真实Node夹具。系统已有Chromium，项目依赖按锁文件安装。
- Cursor Agent不可用。critique采用两个独立Codex评估者，A在B检测结果进入综合前完成。
- 本地现有surface brief缺少目标元信息，按其现有内容保留。无自动hook，使用手动detector；不顺带修复Impeccable配置漂移。

## 评估范围与证据要求

优先覆盖Web初次设置、消息、任务、频道、设备、设置与恢复，以及官网和文档入口。每个可运行页面检查宽屏、手机窄屏、浅/深色、键盘、长内容、加载/空/错误状态。原生Mobile另做源码边界审查；浏览器窄屏不得冒充iOS或Android原生验收。

## 轮次

| 轮次 | Critique | Audit | 代码与测试 | 浏览器证据 | 剩余问题 | 下一轮自提示 |
| --- | --- | --- | --- | --- | --- | --- |
| 准备 | 尚未执行 | 尚未执行 | 初始化独立工作树与锁定依赖 | 待建立真实页面基线 | 待实际评估 | 先运行真实Node和Chromium，检查主要流程与布局，再由独立评估者执行critique/audit，按实际缺陷选择harden或adapt。 |

## 运行与清理

运行脚本、PID、端口、退出命令和截图目录将在启动后写入。最终关闭本轮启动的进程；不终止已有服务。

## 基线采集校验

首轮截图工具未导出IndexedDB，导致新浏览器context丢失设备身份。该批截图不作为评分证据。工具已改为storageState indexedDB=true，重新建网并采集。

## 基线结果与第一轮自提示

A：工作区20/40，官网18/24，文档23/32。B：11/20，P0=0、P1=3、P2=2。证据在 `.artifacts/impeccable-loop/assessment-{a,b}-baseline/`，另有45张四宽度/双主题基线截图。

第一轮执行harden+adapt：修复请求突发导致读取失败、手机页头重复动作与标题挤压、toast遮挡底栏。已改transport、header、AddDevice与toast布局；构建和已有测试已通过，正在运行真实浏览器回归。

下一轮Prompt：实际渲染还存在3.88:1分段文字对比度和关闭抽屉后焦点丢失；执行harden+polish，以真实键盘和对比度读数验证，不改变Arc身份。

## 第一轮复评

Critique工作区28/40，Audit16/20，P0=0/P1=1/P2=2。官网27/36、文档30/40；评估者扩大了已验证启发式，分母与基线不同，不能按裸分作同口径增长。评估来源 `.artifacts/impeccable-loop/assessment-a-round1/scores.json` 与 `assessment-b-round1/audit.json`。

Round1代码修复：纯读请求8并发、64排队；不重发写请求且控制/长调用不被排队；手机页头保留标题并分行动作；单一添加设备入口；toast避让底栏且可滚动至其下方。构建含Web行为测试/typecheck/Client测试和Arc检查通过。真实e2e frontend-quality通过，45张Round1截图已采集并查看。

运行注意：Core的StaticAssets在进程启动时固定读取产物；每轮构建后必须重启本任务fixture才可评估新代码，不能将旧内存产物截图称为新效果。浏览器必须使用任务persistent context[0]保留不可导出的CryptoKey，不能JSON还原。

第二轮Prompt与执行：用harden+adapt修正剩余的浅色分段对比度与触控尺寸、抽屉关闭焦点、320px官网overflow。网站改用生产preview验证真实搜索index。代码已修改，正在中央构建和类型检查；未复评前不记为达标。

## 第二轮复评与第三轮自提示

Critique工作区28/40；Audit17/20，P0=0/P1=0/P2=2。官网22/32、文档28/40，评估者NA集合变化，非同分母趋势。评估文件为assessment-a-round2与assessment-b-round2。

代码：分段文本语义色修正，44px移动目标，task/AddDevice/搜索焦点恢复，320px站点最小宽度修正。Arc和App/WWW/e2e类型检查、生产构建、静态22页检查通过。frontend-quality浏览器回归通过；keyboard-focus初次因未等待启用失败，增强enabled/focused前置断言后1/1通过，保留全部Enter/Escape/X/草稿断言。Round2采集45张截图；审计独立48例，实测浅色文字6.70:1、深色7.49:1。

下一轮Prompt：最大问题转为帮助发现性与语言/内容负荷，另外修复导航抽屉焦点和小按钮触控尺寸。执行clarify+distill+harden：Bot字段就地错误；折叠AI诊断但保留必要错误/登录；使用指南和排障入口；英文首页声明中文指南；检索结果三行摘要；快速上手渐进披露和搜索失败下一步。

第三轮源码已修改，正在构建与测试。未达到36/40，继续循环，不请求确认。

第三轮构建状态：第一次Web构建进程收到SIGTERM，退出143，日志停在已通过245项Client测试之后；没有把它记为构建通过。已确认进程终态后，用独立process session重跑，以避免共享进程组的清理信号交叉影响。WWW生产构建与静态检查已通过。

第三轮文档验证发现：.md 渲染管线不保留原生details/summary，初次WWW浏览器测试正确失败，不能把源码标签当成已实现折叠。已移除无效标签，改为保留锚点的文档顺序整理：源码构建备用路线移到主要五步之后，认证细节移到实际配置步骤；不引入宽泛HTML解析或新的解析依赖。回归仍核对完整原命令可见，不降低搜索/布局/键盘断言。

## 第三轮复评与第四轮自提示

独立Critique：工作区31/40，官网30/32，文档33/40。Audit16/20，P0=0/P1=0/P2=3。证据在assessment-{a,b}-round3；官网适用启发式分母继续按实际评估记录。文档备用路线修正已通过生产构建与2项e2e，但4173 preview的SSR模块仍是此前版本，第三轮文档内容结论以实际加载版本为限，下轮重启后验收。

验证：Round3 Web完整构建真实exit0，Client245项、transport20项通过；App浏览器2/2、修正后WWW浏览器2/2，Arc与相关类型检查通过。退出143的先前运行仍记为中断，不当作通过。

第四轮Prompt：Bot填写草稿经“去设置”返回后丢失，设置断开确认关闭后焦点回BODY，移动目录/页头还有小目标；文档16条平级链接需要按使用任务分组。执行harden+adapt+distill：保留草稿、统一字段错误、恢复焦点、扩大实际点击区；以唯一registry生成4组原生目录，保持16个URL。重新构建并重启preview后，先复现手机搜索，再决定是否需要源码修复；不将未经诊断的观察直接当根因。

第四轮回归进展：Bot草稿修复的5项聚焦行为测试通过；完整Web 425项测试暴露旧TaskHistory测试只检查preventDefault、没有检查实际焦点。改为核对真实opener ref被focus、导航不抢焦点，425项及完整Web构建随后exit0。四组目录9项单元测试及WWW构建通过。新浏览器用例真实发现Fumadocs手机目录没有Escape/焦点管理，正在通过公共useSidebar添加小型可清理适配，不改vendor。关闭流程验证前不把搜索问题标为修复。

帮助入口沿用现有AI设置的GitHub文档路径：新站点帮助域名在环境内重定向失败，GitHub使用指南实测HTTP200；不修改部署或DNS。

第四轮独立正确性复审另发现并修复了新增草稿保持的边界：未知创建在后台refresh确认为同ID Bot后，旧payload不能再成为新建草稿。独立pendingId ref在抽屉隐藏时也追踪，仅对应ID在bots中确证才清理；明确拒绝且无对应Bot继续保留可编辑内容。4组open/closed × confirmed/rejected回归和独立正向复现均通过；review addendum记录resolved，无新增P0/P1/P2。

第四轮验证：完整Web构建exit0，Web430/430与Client245/245；App真实浏览器2/2（含Bot去设置/Back保留与明确取消清空、task字段错误、全部相关抽屉焦点）；WWW浏览器2/2（含4组导航、Tab/Shift+Tab循环、Escape关闭/还焦、搜索结果/搜索503恢复/源码备用路径）。WWW9项单元测试、22HTML/25输出静态检查、相关Web/WWW/Mobile/e2e类型检查及check:ui通过，Arc/vendor/token未改动。

浏览器纠错记录：Fumadocs关闭目录时保留invisible aside，所以测试必须检查hidden和入口焦点，不能要求DOM计数为0；原生实现确实缺少Escape处理的复现与该断言纠错分别留证。新适配的真实Escape关闭与随后手机搜索打开已确认。一次新增App测试操作顺序错误已纠正，不屏蔽原断言。

Round4根代理采集45张多尺寸截图，无页面横向溢出；另实际查看表单、导航、搜索正常/失败图。跨CDP连接可能重置媒体模拟，文件名中的主题不是实际主题证明；独立A/B均改用单连接、读取computed theme并补拍settled帧。评估者尚在独立运行，未填写第四轮分数或宣布达标。

## 第四轮实际复评与第五轮自提示

独立A：工作区29/40，官网25/28（5、7、10不适用），文档34/40；B17/20，P0=0/P1=1/P2=2。A新增App裸Not Found、空任务冗余、同篇搜索段落过多、GitHub主帮助入口4项P2；B新增可选提示4.116:1的P1、频道关闭焦点与任务/设备首读CLS。分数如实记录，不能因上一轮已修复其他问题就维持或调高旧分；适用分母和采样范围变化均保留。

第五轮Prompt：先修可重复的对比度和数据首读稳定性，再处理导航死端与重复内容。执行harden+optimize+distill+clarify：App中文404+返回消息；Arc已有secondary文字色用于support/placeholder；频道焦点和字段验证；首读完成后一次呈现设备/任务，保留错误/离线重试；真正全空任务才隐藏零统计与筛选，历史/Unknown/筛选无匹配不混淆；每篇搜索只保留首个heading/text且保留全部page入口/排名/锚点；空结果下一步；主帮助指向文档站，设置保留GitHub源文档备用链接。最终必须复测CLS与真实浏览器，不能只按源码推断。

第五轮实现边界：nodesRead按connection+generation绑定已解析节点快照，不修改synced或写入策略；基础AI首读成功/失败均放行，长provider检查不阻塞。独立复查发现离线时不会收到current AI callback，已补明确error/offline同时放行settings gate；任务父loading恢复成功时child需按新workspaceState清旧error，避免陈旧错误再出现。相关聚焦测试通过。

第五轮阶段验证：首个完整Web构建exit0，Web439/439、Client245/245；WWW构建、13项单元测试、静态22HTML/25输出、Arc与相关类型检查通过。App浏览器2/2包括真正创建保存无AI任务后再核对非空筛选与对比度，空态入口在首屏、中文404恢复、两个新频道入口的错误及还焦；WWW2/2包括不同文档命中与无结果提示。离线gate最后小修后正在重构建，复评分数仍待独立执行。

第五轮收尾：独立代码复审发现overview/conversation首读失败而workspace_state已成功为空时会持续loading；现已使用未synced时的workspace.error作持久错误，真正重试workspace.refresh，已知Unknown与安全待处理保持可见。回归12项与独立真实TSX harness均通过，无未解决P0/P1/P2代码发现。

一次final构建因tasks-overview在构建中仍有修改，build-input校验按设计拒绝；其exit1及随后夹具启动失败均未计通过。源码稳定后完整重跑真实exit0（round5-web-build-stable），Web445/445、Client245/245；重启夹具后浏览器2/2（round5-app-e2e-stable）通过。独立A/B Round5已启动，不提前填分。

实际浏览器复测（本机受控环境，非field CWV）：390px任务CLS两次0.06255、设备两次0.06306；1440px约0.02251/0.02438。可见“可选”提示两处实测7.114:1，修复此前4.116:1。截图及原始layout-shift sources在round5/focused-checks.json，根代理实际查看empty任务、设备、404和按文档收拢的搜索截图。

共享fixture现有真实保存的任务/频道记录（未启用AI，未调用模型）可供独立评估检查非空列表、筛选和详情；fixture-scenarios.json仅记录安全测试URL。全空场景由当前源码的隔离浏览器回归及截图单独覆盖。没有重置用户数据或原审计工作树。

## 第五轮实际复评与第六轮自提示

独立A5：工作区34/40、官网30/32、文档38/40；B5：17/20，P0=0/P1=0/P2=3。已查看真实非空记录后，发现后台工作queued/message UUID/来源长ID、任务筛选与历史搜索空结果文案、320px频道阅读区不足，以及首读saved tasks绕过gate导致部分数据插入的位移。官网/文档已满足比例要求，App仍不达标，继续修代码而不是保留旧分或宣布完成。

第六轮Prompt：执行clarify+harden+adapt+optimize。后台主文中文化并显示设备名，原始标识进入折叠技术详情且anywhere换行；筛选无匹配统一单个状态并清除筛选，记录搜索区分未读/失败/无匹配/真正空与截断；未运行上下文给出可读原因与下一步。移动频道说明折叠、空闲composer紧凑但保留目标/队列/接收语义。首读以synced或准确nodesRead加业务投影判定就绪，失败/离线仍显示已知记录和恢复。ShellSkeleton的可选“换个方式连接”移动到内容后，消除该68px块在阶段切换时推动骨架的根因。

第六轮实现与验证：任务边界14项focused、独立正确性复审22项通过，无确认P0/P1/P2。完整Web447/447、Client245/245及生产构建exit0，source_changed=false；e2e类型检查、App浏览器2/2通过。浏览器用例新增真实任务/频道保存、未运行上下文人话、任务/频道搜索清除、320已保存任务CLS<=0.1/段落无横向溢出、清除筛选，以及320x740频道history>=160px。并未删除旧对比度/键盘/草稿/404断言。

正在复测持久夹具的首读位移和频道实际布局并查看截图，之后独立A6/B6。第五轮候选zip仍标candidate，最终源码和评估通过后重新绑定制品；尚未commit/push/合并/部署。

第六轮实测收尾：320x740频道历史区218.42px；390为415.42px，1440为355.44px，真实截图已查看。保存任务/设备首读CLS在320两次约0.000109/0.000250、390约0.000090/0.000603、1440约0.00348/0.00370。本机实验不等于field CWV。focused-browser校验中的唯一源变化是loop-state记录写入，目标源码哈希不变。

补充数据核对：总览等待2实际对应1条业务任务+1条后台回复。真实scope_jobs与业务task关联，但当前overview job projection不含scope，不能凭会话/文本推断隐藏未知记录；未发现重复业务记录。保留现有任务与回复活动计数，将摘要明确命名「任务与后台回复」。证据在round6-tasks/count-correlation*，没有为简化数字改变业务逻辑。

最后标题澄清后完整Web重新构建exit0，source_changed=false（round6-web-build-final）。A6/B6已开始独立评估，根不同时连接CDP，不修改UI源码或运行构建，以免污染主题/性能证据。尚未填写第六轮分数。

## 第六轮实际复评与第七轮收尾

A6工作区37/40、官网31/32、文档38/40，均达到比例门槛；B6为19/20，无P0/P1/P2，仅空闲textarea边框造成2px滚动条P3。A仍有2项P2：Bot执行工作抽屉被称为任务记录，易与任务对话混淆；320px展开内联上下文后历史只剩72.625px。设备通用搜索偏向CI内容属于P3。继续修P2而非仅凭分数结束。

第七轮执行clarify+adapt+polish：Bot入口/标题改「执行记录」，说明范围并提供任务对话入口，保留原执行工作/未知状态；手机上下文改为Arc完整阅读抽屉、真实Trigger还焦，聊天/草稿/目标/搜索保持挂载；桌面继续内联；手机低频任务操作折叠，textarea补上原有上下边框高度。独立代码审查未确认P0/P1/P2。

最终版本已安全对齐主线e2093b53：保存46文件逐字副本、patch与指定stash后fast-forward，再应用本轮修改；仅entry-url测试自动合入上游改动，无冲突、无丢失。旧审计工作树与主checkout不变。所有本轮源码备份见integration/20261117T080004Z；不删除用户原有工作。

集成验证：CLI/Core按该主线真实构建且复制到本任务私有runtime（Rust源码无本轮修改）；Core加密并发专项18/18、命令策略3/3通过。完整Web447/447、Client245/245、WWW13/13、Mobile28/28，相关类型检查、Arc guard、22HTML/25输出静态检查及生产构建均通过。第一次Web测试把新增任务导航也计作执行历史链接，正确失败；断言改为仍精确核对4条执行历史且另有1条任务导航，不降低原历史/未知记录断言。

最新Core+CLI+App的真实浏览器2/2、WWW2/2通过（round7-app-e2e/round7-www-e2e）。根实际查看320截图：上下文抽屉320×740、关闭后历史207.64px、焦点回「上下文和历史」；执行记录说明清楚且「查看任务对话」实际回到任务页。当前已无已知未修P2，等待A7/B7独立确认；不提前填分。
