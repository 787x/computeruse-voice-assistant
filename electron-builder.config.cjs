/**
 * electron-builder 配置（CJS：package.json 是 type=module，.js 配置会当成 ESM 解析失败）。
 *
 * 打包分层：
 *   app.asar     代码（apps/ packages/）+ 生产依赖
 *   resources/   模型（models/）与语音运行时（.runtime/）
 *
 * .runtime 有 5GB（Python + torch + 音色包），默认**不打进安装包**：
 * 装完先能唤醒、能识别、能操作电脑，只是没声音；需要发声时把 .runtime 放到安装目录的
 * resources/ 下即可。想一次打全就 CUA_VA_BUNDLE_RUNTIME=1 再构建。
 */
const bundleRuntime = process.env.CUA_VA_BUNDLE_RUNTIME === '1'

const extraResources = [
  { from: 'models', to: 'models' },
  { from: 'packages/voice/gsv_worker.py', to: 'gsv_worker.py' },
]
if (bundleRuntime) extraResources.push({ from: '.runtime', to: '.runtime' })

/** @type {import('electron-builder').Configuration} */
module.exports = {
  appId: 'local.voiceassistant.langbao',
  productName: '语音助手',
  copyright: '个人项目，仅本机使用',
  directories: {
    output: 'dist',
    buildResources: 'apps/shell/assets',
  },
  // 只打运行需要的东西：模型和 .runtime 走 extraResources
  files: [
    'apps/**/*',
    'packages/**/*',
    '!packages/**/*.md',
    '!**/__pycache__/**',
    '!**/*.map',
    'package.json',
  ],
  extraResources,
  // 原生模块（sherpa-onnx）必须从 asar 里解出来，C++ 侧读不到 asar 虚拟文件系统
  asarUnpack: [
    '**/*.node',
    'node_modules/sherpa-onnx-node/**',
    'node_modules/sherpa-onnx-win-x64/**',
    // 外部进程（powershell / python）读不到 asar，这些脚本必须解到 app.asar.unpacked：
    //   cua.ps1     —— 桌面操作桥接（不解放就整个"操作电脑"瘫痪，实测报路径不存在）
    //   foreground.ps1 / ocr.ps1 —— 全屏判定等小工具
    'packages/cua/native/**',
    'packages/system/**',
  ],
  win: {
    target: ['nsis'],
    artifactName: '语音助手-${version}-setup.${ext}',
    /**
     * 关掉 rcedit/签名：这台机器没有代码签名证书，而且 electron-builder 解压
     * winCodeSign 包时要创建符号链接（darwin 的 dylib），普通用户没有这个权限，
     * 会直接报 "Cannot create symbolic link" 把整个构建拦下来。
     * 代价是 exe 不带自定义图标；本地自用无所谓，将来要发布再开 Developer Mode 或买证书。
     */
    signAndEditExecutable: false,
  },
  nsis: {
    oneClick: false,
    perMachine: false,
    allowToChangeInstallationDirectory: true,
    createDesktopShortcut: true,
    createStartMenuShortcut: true,
    shortcutName: '语音助手',
  },
}
