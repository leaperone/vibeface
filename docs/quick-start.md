---
title: 快速上手
description: 从安装 Nightly 到创建个人网络，让第一台电脑上的 Bot 回答一条消息。
---

# 快速上手

这条路线先连接一台电脑，创建个人网络，再让 Bot 回答第一条消息。已有个人网络时，直接看[添加另一台设备](onboarding.md#加入已有网络)，不要重复建网。

## 准备一台电脑

- 使用 Apple Silicon Mac，或运行 glibc 的 Linux x64、Linux ARM64。
- 安装准备使用的 Agent，例如 Codex 或 Claude Code，并准备自己的模型服务和额度。
- 准备一个已有的本机项目目录。首次尝试可以使用没有重要数据的练习项目。

Vibeface 不提供模型账号，也不安装 Agent。它默认使用自己的 Provider 运行目录，原来在终端登录过不代表这里已经登录。后面会从设备设置复制对应的登录命令。

## 1. 安装 Nightly

首次安装可在终端执行以下命令。已经安装时，不重复运行安装器，按[更新说明](install.md#选择更新渠道)管理现有安装。

```sh
export VIBEFACE_INSTALL_ROOT="$HOME/.local/share/vibeface"
export VIBEFACE_BIN_DIR="$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh -o /tmp/vibeface-install.sh
bash /tmp/vibeface-install.sh --channel nightly \
	--install-root "$VIBEFACE_INSTALL_ROOT" \
	--bin-dir "$VIBEFACE_BIN_DIR"
export PATH="$VIBEFACE_BIN_DIR:$PATH"
vibeface --version
```

安装器校验下载包，安装 CLI、Core 和网页资源。看到版本号后，说明 CLI 可以启动。把命令目录加入自己的 shell 配置，才能在新终端继续使用 `vibeface`。平台选择、校验和其他安装方式见[安装与管理节点](install.md)。

## 2. 启动并打开本机页面

```sh
vibeface
```

保持这个终端运行，在同一台电脑的浏览器打开 [http://localhost:4790](http://localhost:4790)。默认使用 `~/.vibeface` 保存本机身份和配置，只监听本机网页入口。首次启动不启用模型，也不授予项目目录。

手机里的 `localhost` 指手机自己，不能用它访问这台电脑。

## 3. 创建个人网络

1. 选择 **新建我的网络**，给这台电脑起一个能认出的名字。浏览器名称已有默认值，可以展开修改。
2. 设置恢复密码，按页面提示达到密码强度要求。
3. 下载恢复文件，确认已保存，再点 **完成创建**。把恢复文件与密码分开保管。

这不是网站账号注册。浏览器、电脑和以后加入的设备分别拥有自己的身份。恢复文件用于找回管理身份，不是项目文件或聊天数据库的完整备份。

## 4. 配置一个能运行 Bot 的 Agent

在 **让这台电脑运行 Bot** 页面选择一个已安装的服务。按该服务显示的提示完成登录，再点 **检查可用性**。

对于默认的 Codex、Claude Code 和 OpenCode，复制页面提供的登录命令，在这台电脑的终端运行。命令会选择 Vibeface 的私有运行目录，不会自动导入你其他会话的登录。不要把登录凭证发到聊天里。

开启该服务的 **运行 Bot**。需要它修改项目时，再开启 **执行任务**，并在 **可用文件夹** 中添加项目的绝对路径。模型未列出时可以手动填写账号实际支持的模型名。详细配置见[首次配置](onboarding.md#配置-agent-和项目目录)。

可以点 **稍后设置**，但创建 Bot 不代表它已经能调用模型。

## 5. 创建 Bot 并验证回答

在 **创建第一个 Bot** 中填写名称，点 **开始对话**。如果服务尚未配置好，按钮显示 **完成**，之后从设备的 **AI** 设置继续配置。

先发送一条不需要修改文件的消息：

> 你好，请用一句话说明你能怎样帮助我。

看到 Bot 的完整回答后，再检查它的项目目录，尝试一个范围明确的任务：

> 列出当前项目的主要目录，并解释 README 中的启动步骤。不要修改文件。

核对回答与实际项目是否一致。允许执行的 Agent 可以访问宿主机，项目目录不是操作系统沙箱。重要操作仍要检查实际结果，详见[权限说明](privacy-security.md)。

## 接下来

- [首次配置与加入设备](onboarding.md)：把另一台电脑、浏览器或手机加入同一网络。
- [日常使用指南](user-guide.md)：管理 Bot、查看工作状态、取消任务和换设备继续。
- [安装与管理节点](install.md#用-cli-管理后台服务)：把前台进程改成后台服务。
- [排障](troubleshooting.md)：页面打不开、Agent 无法使用或任务结果待确认。
