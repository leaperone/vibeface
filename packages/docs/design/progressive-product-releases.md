# 按平台追加产品发布

同一个产品 Tag 对应一个 GitHub Release。每个平台完成本地校验后，就把自己的安装包、校验文件和来源收据追加到公开 Release，供用户手动下载。最终完整性审计不阻塞已完成平台的下载。

## 发布状态与所有权

发布初始为构建中。唯一创建任务核对精确 Tag、主线源码和 CI 身份后创建 Release。Web、四个完整电脑包、四个桌面包、Android 和 iOS 共十一组发布任务，各自只上传封闭文件集合中的独立名称。生产者自己的局部 `release.json` 不上传为全局清单。

完整 `collect` 与这些上传并行，继续校验原始不可变制品、签名来源、内置运行包与精确文件集合。所有平台上传和审计完成后，唯一完成任务核对公开附件并最后追加全局 `release.json`。该文件出现才表示整套发布完成；发布说明的文字状态只用于展示。

已公开附件名称保持写入一次。重试先查询远端，存在且摘要、大小相同则复用；不同则停止。写入响应未知时先对账，不盲目再次上传；不覆盖、删除重建或移动 Tag。并行任务不读改写同一个进度文件或 Release 正文。

## 调用接口

```sh
python3 -B scripts/release.py begin-product
python3 -B scripts/release.py publish-part --kind desktop --target x86_64-pc-windows-msvc --input .artifacts/part
python3 -B scripts/release.py publish product .artifacts/release
```

`publish-part` 的 kind 为 app、native、desktop、android 或 ios；native、desktop 需要精确目标架构。每组验证原制品的版本、Tag、源码、目标、文件集合及摘要后才上传，不能用任意文件路径列表绕过这些约束。

## 自动消费与恢复

新版 CLI、安装器跳过缺少完整清单的构建中 Release。npm 和正式站点部署在最终完成任务之后放行。已安装旧客户端无法被追溯修改；它可能先更新已具备完整电脑包和校验文件的平台，缺少目标文件时失败并保留原安装。

自动恢复只消费同一原运行中已验证的不可变制品。平台附件已经公开后，不能重建并覆盖相同 Tag 的字节；原制品丢失、过期或摘要冲突时停止恢复。独立官网继续自己的发布组，不加入产品平台追加任务。

公开附件仍采用既有全局清单格式和文件集合。数据 epoch、更新协议保持 1；这次调整不改变产品版本、设备身份或数据格式。仓库需允许 Release 附件追加，Tag 的不可变保护继续保留。
