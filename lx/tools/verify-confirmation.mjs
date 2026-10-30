import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
const source = await readFile(new URL('../apps/shell/main.mjs', import.meta.url), 'utf8').then(s => s.replace(/\r\n/g, '\n'))
function section(start, end) { return source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start))) }
const timers = []
const ctx = vm.createContext({ console, setTimeout: fn => (timers.push(fn), timers.length), clearTimeout() {} })
vm.runInContext(`
let confirmResolver=null, confirmSequence=0, pendingConfirmId=null, pendingConfirmCapability=null;
let commandActive=false, commandTimer=null, asr={reset(){},flush(){return '确认吗？'}}, lastCommandText='',lastSentPartial='';
let voiceConfirmEnabled=true, heardAny=false, commandStartAt=0, hideTimer=null, activeTurn=null,currentState='hidden';
let followUpWindowStart=0, grants=0, permissionGate={grantOnce(){grants++;return Date.now()}};
let win={isDestroyed:()=>false,webContents:{send(){}, mainFrame:{}}};
const CONFIRM_TIMEOUT_MS=12000;
function describeRequest(){return {text:'test',detail:'test'}}
function stopSpeaking(){} function setState(s){currentState=s} function sendSubtitle(){} function log(){}
function parseConfirmText(){return {allow:true,channel:'voice'}}
${section('async function askUser(request)', '/**\n * 收掉卡片')}
${section('function closeConfirmCard()', '\n}').concat('\n}')}
${section('function onWake(keyword)', '\n}').concat('\n}')}
${section('function finishCommand(reason)', '\n}').concat('\n}')}
let ipcHandler;
const ipcMain={on(name,handler){ipcHandler=handler}};
${section("  ipcMain.on('hud:confirm',", '\n\n  // 麦克风')}
`, ctx)
const run = code => vm.runInContext(code,ctx)
run("var p=askUser({capability:'clipboard.read'}); var first=pendingConfirmId")
run("onWake('wake'); finishCommand('endpoint')") // injected affirmative parser must leave card usable
assert.equal(run('confirmResolver !== null && commandActive'),true)
run("settleConfirm({allow:true,channel:'voice',once:true})")
assert.equal(run('grants'),0)
run("voiceConfirmEnabled=false; onWake('wake'); settleConfirm({allow:false,channel:'voice'})")
assert.equal(run('confirmResolver !== null'),true)
run("ipcHandler({sender:{},senderFrame:win.webContents.mainFrame},{id:first,answer:true})")
assert.equal(run('confirmResolver !== null'),true)
run("var q=askUser({capability:'clipboard.read'}); var second=pendingConfirmId")
assert.equal((await run('p')).allow,false)
timers[0]() // stale timeout cannot cancel replacement
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:first,answer:true})")
assert.equal(run('confirmResolver !== null'),true)
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:second,answer:'once'})")
assert.equal((await run('q')).allow,true)
run("ipcHandler({sender:win.webContents,senderFrame:win.webContents.mainFrame},{id:second,answer:'once'})")
assert.equal(run('grants'),1)
run("var r=askUser({}); closeConfirmCard()")
assert.equal((await run('r')).allow,false)
run('var t=askUser({})'); timers.at(-1)()
assert.equal((await run('t')).channel,'timeout')
run("var u=askUser({}); voiceConfirmEnabled=true; settleConfirm({allow:false,channel:'voice'})")
assert.equal((await run('u')).allow,false)
console.log('Confirmation lifecycle: PASS (faulty parser, disabled voice, wake, stale timeout/click, sender, repeat, grant, close, timeout, cancel)')
