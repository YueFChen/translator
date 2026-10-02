# 发布翻译工作台

本插件仅在宿主机本地使用，需要 Core 0.1.8 或更新的 0.x 版本。发布流程只构建 Git 提交内容，不包含开发者工作区中未提交的修改。

CI 固定 Core SDK 提交，执行冻结安装、UI 类型检查和构建、Rust 格式检查、Clippy、测试、生成绑定检查以及 Windows x86_64 发布打包。`pnpm build:release` 生成 `target/translator-<version>-windows-x86_64.wplug`，并逐文件生成 SHA-256 清单。

首次发布前运行 `node scripts/generate-update-signing-key.mjs`，将 `.secrets/PLUGIN_UPDATE_SIGNING_KEY` 通过标准输入保存为仓库 Actions secret `PLUGIN_UPDATE_SIGNING_KEY`，将公钥文件内容保存为仓库 variable `PLUGIN_UPDATE_SIGNING_PUBLIC_KEY`。私钥文件不得输出或提交。已有密钥不得覆盖。

确认对应提交 CI 成功、Core 最低兼容版本已发布后，推送与四处版本元数据一致的 `vX.Y.Z` 标签。Release 工作流复用 CI 已验证的安装包，使用 Ed25519 签署 `translator-update.json`，并发布两个资产。签名清单固定安装包哈希、大小、兼容性与能力信息。

首次公开发布通过 Catalog 校验后，提交 `translator` 身份登记 PR；目录需要维护者人工审核。后续版本无需修改目录身份记录。不得移动已发布标签。
