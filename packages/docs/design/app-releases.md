# 应用与产品发布边界

Core、CLI、Web App 和 Mobile 组成同一个产品发布组。`v<version>` 固定同一版本与源码，在一次并行流水线中构建，并在一个 GitHub Release 交付。官网保持独立的 `www-v<version>`。

构建和失败恢复约束见 [产品组合发布](product-releases.md)，操作与制品契约见 [构建与发布](../releasing.md)。