import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
import { NativeBridge } from '../packages/cua/bridge.js'
import { CuaToolkit } from '../packages/cua/adapter.mjs'
import { AgentInitialization } from '../packages/agent/initialization.mjs'
const tick=()=>new Promise(r=>setImmediate(r))
function fake() {
 const children=[],commands=[];let kills=0,live=0
 const spawnImpl=()=>{
  const child=new EventEmitter();child.stdout=new EventEmitter();child.stderr=new EventEmitter()
  child.stdin={end(){},write(line){commands.push(JSON.parse(line))}}
  child.exitCode=null;child.signalCode=null;live++
  child.kill=()=>{kills++;queueMicrotask(()=>{live--;child.signalCode='SIGTERM';child.emit('exit',null,'SIGTERM')})}
  children.push(child);return child
 }
 return {bridge:new NativeBridge({scriptPath:'test.ps1',spawnImpl}),children,commands,
  ready(){children[0].stdout.emit('data',JSON.stringify({event:'ready',id:null,data:{protocol:1}})+'\n')},
  get live(){return live},get kills(){return kills}}
}
{
 const f=fake();await f.bridge.dispose();await f.bridge.dispose()
 await assert.rejects(f.bridge.ensureStarted());assert.equal(f.children.length,0)
}
for(const phase of ['starting','banner-race','pending-call']) {
 const f=fake();const warmup=f.bridge.ensureStarted();const rejected=phase==='pending-call'?null:assert.rejects(warmup)
 if(phase!=='starting')f.ready()
 if(phase==='pending-call'){
  await warmup
  const first=f.bridge.call('ping'),second=f.bridge.call('ping')
  const r1=assert.rejects(first),r2=assert.rejects(second);await tick()
  await f.bridge.dispose();await Promise.all([r1,r2]);assert.equal(f.commands.length,1)
 }else{
  const queued=f.bridge.call('ping');const rq=assert.rejects(queued)
  await f.bridge.dispose();await Promise.all([rejected,rq])
 }
 await f.bridge.dispose()
 assert.equal(f.live,0);assert.equal(f.kills,1)
 await assert.rejects(f.bridge.ensureStarted());assert.equal(f.children.length,1)
}
{
 const f=fake();const startup=f.bridge.ensureStarted();const rejected=assert.rejects(startup)
 const kill=f.children[0].kill
 f.children[0].kill=()=>false
 await assert.rejects(f.bridge.dispose(),/did not exit/)
 await rejected
 f.children[0].kill=kill
 await f.bridge.dispose()
 assert.equal(f.live,0);assert.equal(f.kills,1)
}
console.log('Bridge lifecycle: 5 scenarios PASS (mock children, exit/kill/command counts asserted)')
if(process.argv.includes('--real')) {
 assert.equal(process.platform,'win32','real PowerShell verification requires Windows')
 for(const phase of ['cancel-starting','close-starting','close-ready','shell-shutdown']) {
  let spawnedResolve
  const spawned=new Promise(r=>spawnedResolve=r),children=[]
  let disposals=0
  const owner=new AgentInitialization(),controller=new AbortController()
  const pending=owner.ensure(controller.signal,async({own,checkpoint})=>{
   const toolkit=own(new CuaToolkit({spawnImpl:(...args)=>{
    const child=spawn(...args);children.push(child);child.once('spawn',()=>spawnedResolve(child));return child
   }}))
   const dispose=toolkit.dispose.bind(toolkit)
   toolkit.dispose=async()=>{disposals++;await dispose()}
   await toolkit.warmup();checkpoint();return {toolkit}
  })
  const rejected=['close-ready','shell-shutdown'].includes(phase)?null:assert.rejects(pending)
  await spawned
  if(['close-ready','shell-shutdown'].includes(phase))await pending
  if(phase==='cancel-starting')controller.abort()
  if(phase==='shell-shutdown') {
   const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
   const start=source.indexOf('async function shutdown()')
   const code=source.slice(start,source.indexOf('\n}',start)+2)
   let releaseMic,exitCode
   const mic=new Promise(r=>releaseMic=r)
   const ctx=vm.createContext({agentInitialization:owner,setTimeout,clearTimeout,ttsEngine:null,
    cancelActiveTurn(){},log(){},stopListening:()=>mic,stopSpeaking(){},app:{exit(code){exitCode=code}}})
   vm.runInContext(code,ctx)
   const stopped=ctx.shutdown()
   await new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>reject(Error('bridge remained alive while shutdown waited for mic')),2000)
    children[0].once('exit',()=>{clearTimeout(timer);resolve()})
   })
   assert.equal(exitCode,undefined,'cleanup must happen before final app exit')
   releaseMic();await stopped;assert.equal(exitCode,0)
  }else await owner.close()
  if(rejected)await rejected
  await tick()
  assert.equal(owner.runtime,null)
  assert.equal(disposals,1)
  assert.equal(children.length,1)
  for(const child of children) {
   assert.ok(child.exitCode!==null || child.signalCode!==null,'owned native child must report exit')
   let alive=true
   try{process.kill(child.pid,0)}catch(error){assert.equal(error.code,'ESRCH');alive=false}
   assert.equal(alive,false,`owned PowerShell PID ${child.pid} must no longer exist`)
  }
  console.log(JSON.stringify({suite:'real PowerShell lifecycle',phase,pass:true,created:children.length,disposed:disposals,remainingOwnedPids:0}))
 }
}
