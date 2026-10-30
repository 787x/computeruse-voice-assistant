// Run with an Electron executable: electron tools/verify-electron-confirmation.mjs
// This harness loads production preload/renderer and confirmation functions, not the app bootstrap.
import { app, BrowserWindow, ipcMain } from 'electron'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import vm from 'node:vm'
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync } from 'node:fs'
const root=dirname(dirname(fileURLToPath(import.meta.url)))
mkdirSync(join(root,'.tmp'),{recursive:true})
const report=value=>{const text=JSON.stringify(value);appendFileSync(join(root,'.tmp/electron-confirmation-results.log'),text+'\n');console.log(text)}
report({stage:'start'})
const watchdog=setTimeout(()=>{report({pass:false,error:'test watchdog timeout'});app.exit(1)},15000)
const isolated=mkdtempSync(join(tmpdir(),'confirmation-electron-'))
app.setPath('userData',isolated)
const source=(readFileSync(join(root,'apps/shell/main.mjs'),'utf8')).replace(/\r\n/g,'\n')
const fn=name=>{const a=source.indexOf(name);assert.ok(a>=0,name);return source.slice(a,source.indexOf('\n}',a)+2)}
const sleep=ms=>new Promise(r=>setTimeout(r,ms))
let win
async function main() {
try {
 await app.whenReady();report({stage:'ready'})
 win=new BrowserWindow({width:500,height:400,show:false,webPreferences:{
  preload:join(root,'apps/shell/preload.mjs'),contextIsolation:true,nodeIntegration:false,sandbox:false,
 }})
 const ctx=vm.createContext({win,setTimeout,clearTimeout,Date,AbortController,console,ipcMain})
 vm.runInContext(`
 let confirmResolver=null,confirmSequence=0,pendingConfirmId=null,pendingConfirmCapability=null;
 let confirmVoiceEpoch=0,voiceConfirmEnabled=false,listening=false,asr=null,commandTimer=null,commandActive=false;
 let heardAny=false,lastCommandText='',lastSentPartial='',commandStartAt=0,hideTimer=null,currentState='hidden',activeTurn=null;
 let grants=0; const permissionGate={grantOnce(){grants++;return Date.now()}};
 const CONFIRM_TIMEOUT_MS=2500;
 function log(){} function stopSpeaking(){} function sendSubtitle(){}
 function setState(state){currentState=state;win.webContents.send('hud:state',state)}
 function describeRequest(){return {text:'TEST ONLY: permission verdict, no tool execution',detail:'read_clipboard'}}
 ${fn('function syncConfirmVoice()')}
 ${fn('async function askUser(request)')}
 ${fn('function settleConfirm(value, id = pendingConfirmId, trustedClick = false)')}
 ${fn('function closeConfirmCard()')}
 ${source.slice(source.indexOf("  ipcMain.on('hud:confirm',"),source.indexOf('\n\n  // 麦克风',source.indexOf("  ipcMain.on('hud:confirm',")))}
 `,ctx)
 const run=code=>vm.runInContext(code,ctx)
 await win.loadFile(join(root,'apps/shell/renderer/index.html'))
 report({stage:'loaded'});await sleep(150)
 const click=async id=>{
  const point=await win.webContents.executeJavaScript(`(()=>{const r=document.getElementById(${JSON.stringify(id)}).getBoundingClientRect();return {x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)}})()`)
  win.webContents.sendInputEvent({type:'mouseDown',button:'left',clickCount:1,...point})
  win.webContents.sendInputEvent({type:'mouseUp',button:'left',clickCount:1,...point})
 }
 const pending=run('askUser({capability:"clipboard.read"})')
 await sleep(100)
 assert.equal(await win.webContents.executeJavaScript('typeof window.hud.answerConfirm'),'undefined')
 await win.webContents.executeJavaScript(`
  for(const id of ['allow','allowOnce','deny']) {
   document.getElementById(id).click();
   document.getElementById(id).dispatchEvent(new MouseEvent('click',{bubbles:true}));
  }
  window.hud.onConfirm(card=>{ card.id=999999; });
 `)
 await sleep(80)
 assert.equal(run('confirmResolver!==null'),true,'page script must not settle request')
 assert.equal(run('grants'),0)
 await click('allow')
 assert.equal((await pending).allow,true,'trusted Chromium input should approve')
 assert.equal(run('confirmResolver'),null)
 const once=run('askUser({capability:"clipboard.read"})')
 await sleep(80);await click('allowOnce');assert.equal((await once).once,true)
 assert.equal(run('grants'),1)
 await click('allowOnce');await sleep(80);assert.equal(run('grants'),1,'duplicate cannot grant again')
 const deny=run('askUser({})');await sleep(80);await click('deny');assert.equal((await deny).allow,false)
 const keyboard=run('askUser({})');await sleep(80)
 await win.webContents.executeJavaScript(`document.activeElement?.blur();window.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}))`)
 await sleep(40);assert.equal(run('confirmResolver!==null'),true,'synthetic keyboard must not approve')
 win.webContents.sendInputEvent({type:'keyDown',keyCode:'Enter'})
 win.webContents.sendInputEvent({type:'keyUp',keyCode:'Enter'})
 assert.equal((await keyboard).allow,true,'trusted Enter must work without a mouse click')
 const timeout=run('askUser({})');assert.equal((await timeout).channel,'timeout')
 const controller=new AbortController();ctx.controller=controller
 const cancelled=run('askUser({signal:controller.signal})');controller.abort()
 assert.equal((await cancelled).channel,'cancelled')
 clearTimeout(watchdog)
 report({suite:'Electron isolated preload',electron:process.versions.electron,
  pass:true,checks:['public approval API absent','synthetic clicks rejected','page callback mutation isolated','trusted input allowed','once','duplicate','deny','trusted Enter','synthetic Enter rejected','timeout','abort']})
 app.exit(0)
} catch(error) {
 report({pass:false,error:error.stack})
 console.error(error.stack)
 app.exit(1)
}

}
void main()
