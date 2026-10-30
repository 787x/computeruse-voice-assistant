import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
import { NativeBridge } from '../packages/cua/bridge.js'
import { CuaToolkit } from '../packages/cua/adapter.mjs'
import { AgentInitialization } from '../packages/agent/initialization.mjs'
const tick=()=>new Promise(r=>setImmediate(r))
function fake(options = {}) {
 const children=[],commands=[];let kills=0,live=0
 const spawnImpl=()=>{
  const child=new EventEmitter();child.stdout=new EventEmitter();child.stderr=new EventEmitter()
  child.stdin={end(){},write(line){commands.push(JSON.parse(line))}}
  child.exitCode=null;child.signalCode=null;live++
  child.kill=()=>{kills++;queueMicrotask(()=>{live--;child.signalCode='SIGTERM';child.emit('exit',null,'SIGTERM')})}
  children.push(child);return child
 }
 return {bridge:new NativeBridge({scriptPath:'test.ps1',spawnImpl,...options}),children,commands,
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
// Startup/request failures retain the live child, refuse restart and allow a later cleanup.
for(const phase of ['startup-false','startup-throw','request-timeout','request-abort']) {
 const f=fake({startupTimeoutMs:10,defaultTimeoutMs:10})
 const started=f.bridge.ensureStarted(),child=f.children[0],kill=child.kill
 child.kill=()=>{if(phase==='startup-throw')throw Error('kill denied');return false}
 if(phase.startsWith('startup'))await assert.rejects(started)
 else {
  f.ready();await started
  const controller=new AbortController()
  const call=f.bridge.call('ping',{}, {signal:controller.signal})
  const rejected=assert.rejects(call)
  await tick();if(phase==='request-abort')controller.abort()
  await rejected
 }
 assert.equal(f.bridge.alive,true)
 assert.ok(f.bridge.terminationFailure)
 await assert.rejects(f.bridge.ensureStarted(),/previous backend/)
 assert.equal(f.children.length,1)
 const failed=await Promise.allSettled([f.bridge.dispose()])
 assert.equal(failed[0].status,'rejected')
 assert.equal(f.bridge.alive,true)
 child.kill=kill;await f.bridge.dispose()
 assert.equal(f.bridge.alive,false);assert.equal(f.live,0)
}
// A delayed callback from an exited child cannot corrupt a replacement process/request.
{
 const f=fake({defaultTimeoutMs:10})
 const first=f.bridge.ensureStarted();f.ready();await first
 const old=f.children[0],stdout=old.stdout.listeners('data')[0],error=old.listeners('error')[0]
 await assert.rejects(f.bridge.call('ping'))
 await tick()
 const second=f.bridge.ensureStarted(),child=f.children[1]
 child.stdout.emit('data',JSON.stringify({event:'ready',id:null,data:{generation:2}})+'\n')
 await second
 stdout(JSON.stringify({event:'ready',id:null,data:{generation:1}})+'\n');error(Error('late error'));old.emit('exit',1,null)
 assert.equal(f.bridge.readyInfo.generation,2)
 assert.equal(old.stdout.listenerCount('data'),0)
 const call=f.bridge.call('ping');await tick()
 child.stdout.emit('data',JSON.stringify({id:f.commands.at(-1).id,ok:true,data:{safe:true}})+'\n')
 assert.equal((await call).safe,true)
 await f.bridge.dispose();assert.equal(f.live,0)
}
console.log('Fault ownership / delayed old callbacks: 5 scenarios PASS')
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
   const ctx=vm.createContext({musicCredentials:{close:async()=>({ok:true,remaining:0})},agentInitialization:owner,setTimeout,clearTimeout,ttsEngine:null,
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

// Node spawn failures can close without ever emitting exit.
for (const order of [['error','close'],['error','exit','close'],['exit','close']]) {
 const f=fake(),initial=f.bridge.ensureStarted(),rejected=assert.rejects(initial)
 const child=f.children[0];child.kill=()=>false
 for(const event of order)child.emit(event,event==='error'?Object.assign(Error('missing executable'),{code:'ENOENT'}):1,null)
 await rejected
 assert.equal(f.bridge.alive,false)
 const retry=f.bridge.ensureStarted(),next=f.children[1]
 next.stdout.emit('data',JSON.stringify({event:'ready',id:null,data:{generation:2}})+'\n')
 await retry
 child.emit('close',1,null);child.emit('error',Error('late'))
 assert.equal(f.bridge.readyInfo.generation,2)
 await f.bridge.dispose();assert.equal(f.bridge.alive,false)
}
{
 let attempts=0
 const bridge=new NativeBridge({scriptPath:'test.ps1',spawnImpl:()=>{attempts++;throw Error('spawn throws')}})
 await assert.rejects(bridge.ensureStarted());await assert.rejects(bridge.ensureStarted())
 assert.equal(attempts,2);assert.equal(bridge.alive,false);await bridge.dispose()
}
console.log('Spawn error/close event ordering and synchronous throw: 4 scenarios PASS')
if(process.argv.includes('--real')) {
 let count=0;const children=[]
 const toolkit=new CuaToolkit({spawnImpl:(exe,args,options)=>{
  const child=spawn(count++===0?'codex-intentionally-missing-executable.exe':exe,args,options);children.push(child);return child
 }})
 await assert.rejects(toolkit.warmup())
 await new Promise(resolve=>setTimeout(resolve,50))
 await toolkit.warmup();assert.ok(children[1].pid)
 await toolkit.dispose()
 assert.ok(children[1].exitCode!==null || children[1].signalCode!==null)
 assert.throws(()=>process.kill(children[1].pid,0),e=>e.code==='ESRCH')
 console.log('Real ENOENT error/close, retry with PowerShell and PID cleanup: PASS')
}
