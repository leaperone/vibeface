# 独立应用发布

Core、App、CLI、Desktop、Mobile 和 WWW 分别拥有版本与不可变 `<app>-v<version>` Tag。npm 是 CLI 的分发，不增加第七个产品版本。共享 workspace 包是构建输入，不承担公开产品身份。

依赖图为 Core/App → CLI → Desktop。Mobile 和 WWW 独立构建，Core/App 各自完成后直接部署自己的服务。CLI 把已经发布的 Core 和 App 字节组合成完整运行包；Desktop 再内置一份精确 CLI 包。每个依赖 pin 记录 Tag、版本、源码 SHA 和 manifest SHA256，消费方逐项核对，不推断同源码或同版本关系。

发布器在本应用资产完整后最后写 `release.json`。其他应用缺少资产、签名失败或商店处理延迟，都不阻止无依赖产品发布。父 Release 等待对应服务部署的真实终态，网络响应未知时只读对账，不直接重写。

协调器按构建输入与依赖 pin 变化创建 Hourly；没有变化就复用现有完成版本。只改所选应用的版本和依赖锁，main 固定开发版本不变。旧 Product 标签、全局完成门和旧 updater 兼容分支已删除。

选择按应用锁依赖，而不建设一个全局 cohort registry，是为了用现有 GitHub 制品信任边界提供可重现安装，同时避免重新引入跨产品发布屏障。格式、命令和验收见[构建与发布](../releasing.md)。
