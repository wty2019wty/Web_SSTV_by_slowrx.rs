# 项目约定

面向本仓库的开发约定。通用语言要求见全局 `AGENTS.md`（始终使用简体中文）。

## 环境：Node 必须用项目内置版本

仓库自带便携版 Node（`.node-v24.19.0-win-x64/`，已 gitignore），**不会**写进系统 PATH。
每个新开的终端先执行一次：

```powershell
. .\node_env.ps1
```

之后 `node` / `npm` / `npx` 才可用；直接调用报“无法识别”基本都是漏了这一步。
详见 `README.md` 的「常见问题排查 → 提示找不到 node / npm 命令」。

## 常用命令

```powershell
.\build-wasm.ps1 -Dev     # 构建 wasm（-Dev 含合成测试音频）
npm run build             # 前端生产构建（含 vue-tsc 类型检查）
npm run typecheck         # 仅类型检查
npm run e2e               # 浏览器端到端（需先 -Dev 构建 wasm）
npm run probe:mic         # 麦克风授权探针（验证是否真的调用 getUserMedia）
```

## 代码风格

- 前端：Vue 3 `<script setup>` + TypeScript，单引号、无分号。
- 注释、文档、提交信息均使用简体中文。


