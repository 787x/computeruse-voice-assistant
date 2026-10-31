import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import vm from 'node:vm'
import { AgentInitialization } from '../packages/agent/initialization.mjs'
const source=(await readFile(new URL('../apps/shell/main.mjs',import.meta.url),'utf8')).replace(/\r\n/g,'\n')
const a=source.indexOf('async function ensureAgent(signal)')
assert.ok(a>=0)
const code=source.slice(a,source.indexOf('\n}',a)+2).replaceAll('import(', 'loadModule(')
const deferred=()=>{let resolve,reject;const promise=new Promise((r,j)=>{resolve=r;reject=j});return {promise,resolve,reject}}
const tick=()=>new Promise(r=>setImmediate(r))
let cases=0
function fixture(stage, failure=false, initializer=code) {
 const pause=deferred(),entered=deferred(),controllers=[],resources=[]
 const counts={bridge:0,disposed:0,sync:0,save:0,launcher:0,agent:0,active:0}
 let paused=false
 const wait=async name=>{if(stage===name && !paused){paused=true;entered.resolve();await pause.promise;if(failure)throw Error(`failed ${name}`)}}
 let cachedConfig={netease:{appId:'test'},model:'test'}
 const store={loadConfig:async()=>{await wait('config');return cachedConfig},resolveApiKey:async()=>{await wait('key');return 'test'},resolveSearchKey:async()=>{await wait('search');return ''},saveConfig:async patch=>{counts.save++;cachedConfig={...cachedConfig,...patch}}}
 class Toolkit {
  constructor(){counts.bridge++;counts.active++;resources.push(this)}
  async warmup(){await wait('warmup')}
  async dispose(){counts.disposed++;counts.active--;this.closed=true}
 }
 class Agent {constructor(options){this.tools=options.tools;counts.agent++;if(stage==='agent-failure')throw Error('agent construction failed')}}
 const modules={
  '../../packages/config/store.mjs':store,
  // 壳子给 LLMClient 传提供商档案（决定推理参数），桩件必须提供同一个函数
  '../../packages/agent/llm.mjs':{LLMClient:class{constructor(options){this.options=options}},profileFromConfig:(source)=>({ providerId:source?.providerId ?? '', capability:source?.providerId ?? '' })},
  '../../packages/agent/loop.mjs':{Agent},
  '../../packages/cua/adapter.mjs':{CuaToolkit:Toolkit},
  '../../packages/tools/web-search.mjs':{WebSearch:class{enabled=false}},
 }
 const owner=new AgentInitialization()
 const ctx=vm.createContext({AgentInitialization,DOMException,Date,console,join:()=>'',DATA_ROOT:'test',
 musicCredentials:{close:async()=>({ok:true,remaining:0})},agentInitialization:owner,shuttingDown:false,lastUtterance:'test',
 loadModule:async path=>{await wait(path.includes('/config/')?'modules':'other-module');return modules[path]},
 runNetease:async()=>{counts.play=(counts.play??0)+1;return {text:'ok'}},
 syncCredentialsAsync:async(_credentials,options)=>{if(options.syncedHash==='new')return {synced:[],hash:'new'};counts.sync++;await wait('music');return {synced:[],hash:'new'}},
 lxAvailable:async()=>{counts.sync++;await wait('music');return true},
 ensureLauncher:async signal=>{signal?.throwIfAborted();counts.launcher++;await wait('launcher');return {}},
 log(){},applyModelSchedule(){},handleAgentEvent(){},
 })
 vm.runInContext(initializer,ctx)
 const start=()=>{const c=new AbortController();controllers.push(c);return {c,p:ctx.ensureAgent(c.signal)}}
 return {owner,start,counts,pause,entered,resources,setConfig:value=>cachedConfig=value}
}
// Cancellation at every asynchronous initialization stage must stop following work.
for (const stage of ['modules','config','key','warmup','search','launcher']) {
 const f=fixture(stage), {c,p}=f.start();const rejected=assert.rejects(p,e=>e.name==='AbortError')
 await f.entered.promise;c.abort();await rejected
 f.pause.resolve();await tick();await tick()
 assert.equal(f.owner.runtime,null);assert.equal(f.owner.loading,false)
 assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,f.counts.bridge)
 if(['modules','config','key','music'].includes(stage))assert.equal(f.counts.bridge,0)
 if(stage!=='launcher')assert.equal(f.counts.launcher,0)
 if(['modules','config','key','music'].includes(stage))assert.equal(f.counts.save,0)
 cases++
}
// An already-aborted caller must not even load a module.
{
 const f=fixture('modules'),c=new AbortController();c.abort()
 await assert.rejects(f.owner.ensure(c.signal,()=>{throw Error('must not build')}),e=>e.name==='AbortError')
 assert.equal(f.owner.loading,false);assert.equal(f.counts.bridge,0);cases++
}
for(const stage of ['warmup','search','launcher','agent-failure']) {
 const f=fixture(stage,true),{p}=f.start();const rejected=assert.rejects(p)
 if(stage!=='agent-failure'){await f.entered.promise;f.pause.resolve()}
 await rejected
 assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,1)
 assert.equal(f.owner.runtime,null);assert.equal(f.owner.loading,false)
 if(stage!=='agent-failure'){
  const runtime=await f.start().p;assert.equal(f.owner.runtime,runtime);assert.equal(f.counts.active,1)
 }
 await f.owner.close();assert.equal(f.counts.active,0);cases++
}
// Two consumers can share initialization. Cancelling one cannot destroy the other's resource.
{
 const f=fixture('warmup'),first=f.start();await f.entered.promise
 const second=f.start();const rejected=assert.rejects(first.p,e=>e.name==='AbortError');first.c.abort();await rejected
 assert.equal(f.counts.disposed,0)
 f.pause.resolve();const runtime=await second.p
 assert.equal(f.owner.runtime,runtime);assert.equal(f.counts.bridge,1)
 const third=await f.start().p;assert.equal(third,runtime)
 await f.owner.close();assert.equal(f.counts.active,0);assert.equal(f.counts.disposed,1);cases++
}
// A cancelled job's late finally must not clear/destroy a newer initialization.
{
 const f=fixture('warmup'),old=f.start();await f.entered.promise
 const rejected=assert.rejects(old.p);old.c.abort();await rejected
 const next=f.start();const runtime=await next.p
 f.pause.resolve();await tick();await tick()
 assert.equal(f.owner.runtime,runtime);assert.equal(runtime.toolkit.closed,undefined)
 assert.equal(f.counts.active,1);assert.equal(f.counts.disposed,1)
 await f.owner.close();assert.equal(f.counts.active,0);cases++
}
// Exit during import/warmup and at the publication boundary is terminal and resource-safe.
for(const stage of ['modules','warmup','launcher']) {
 const f=fixture(stage),{p}=f.start();const rejected=assert.rejects(p)
 await f.entered.promise
 const closing=f.owner.close();f.pause.resolve()
 await Promise.all([closing,rejected]);await tick()
 assert.equal(f.owner.runtime,null);assert.equal(f.counts.active,0)
 await assert.rejects(f.start().p,e=>e.name==='AbortError');cases++
}
// Cleanup failure retains ownership for a later exit retry.
{
 let disposed=0,diagnostics=0
 const owner=new AgentInitialization({diagnostic(){diagnostics++}})
 const controller=new AbortController()
 await assert.rejects(owner.ensure(controller.signal,async({own})=>{
  own({dispose:async()=>{disposed++;if(disposed===1)throw Error('transient cleanup failure')}})
  throw Error('initialization failed')
 }))
 assert.equal(diagnostics,1);await owner.close();assert.equal(disposed,2);cases++
}
// Cancelled imports cannot publish into a newer pending initialization.
{
 const owner=new AgentInitialization(),oldPause=deferred(),newPause=deferred()
 const oldController=new AbortController(),newController=new AbortController()
 const old=owner.ensure(oldController.signal,async({checkpoint})=>{await oldPause.promise;checkpoint();return {}})
 const rejected=assert.rejects(old);await tick();oldController.abort();await rejected
 const next=owner.ensure(newController.signal,async()=>{await newPause.promise;return {}})
 oldPause.resolve();await tick();assert.equal(owner.loading,true)
 newPause.resolve();await next;await owner.close();cases++
}
console.log(`Agent initialization ownership: ${cases} scenarios PASS (actual initializer, mocked modules/resources)`)
// Slow unrelated music must not delay ordinary initialization.
{
 const f=fixture('music')
 const runtime=await f.start().p
 assert.equal(f.counts.sync,0)
 assert.equal(f.counts.agent,1)
 await f.owner.close()
 console.log('Non-music initialization with hung music dependency: PASS')
}
// Root-only credential initialization checks: never touch the actual CLI/config.
{
 const { createHash } = await import('node:crypto')
 let musicSource
 try { musicSource=await readFile(new URL('../packages/tools/netease.mjs',import.meta.url),'utf8') }
 catch(error){if(error.code!=='ENOENT')throw error}
 if(musicSource){
  musicSource=musicSource.replace(/\r\n/g,'\n')
  const start=musicSource.indexOf('export async function syncCredentialsAsync(')
  const code=musicSource.slice(start,musicSource.indexOf('\n}',start)+2).replace('export ','')
  for(const phase of ['get','set']){
   const pause=deferred(),entered=deferred(),controller=new AbortController(),calls=[]
   const ctx=vm.createContext({createHash,runCliAsync:async(argv,options)=>{
    assert.equal(options.signal,controller.signal);calls.push(argv[1])
    if(argv[1]===phase){entered.resolve();await pause.promise}
    return {raw:'',code:0}
   }})
   vm.runInContext(code,ctx)
   const pending=ctx.syncCredentialsAsync({appId:'fake',privateKey:'fake'},{signal:controller.signal})
   const rejected=assert.rejects(pending,e=>e.name==='AbortError')
   await entered.promise;controller.abort();pause.resolve();await rejected
   assert.deepEqual(calls,phase==='get'?['get']:['get','set'])
  }
  console.log('Cancelled credential sync: 2 mocked CLI scenarios PASS (no real config writes)')
 }
}

for(const transient of [true,false]) {
 let first=0,second=0
 const owner=new AgentInitialization()
 await assert.rejects(owner.ensure(undefined,async({own})=>{
  own({dispose:async()=>{first++;if(!transient || first===1)throw Error('resource stuck')}})
  own({dispose:async()=>{second++}})
  throw Error('build failed')
 }))
 const result=await owner.close()
 assert.equal(result.ok,transient)
 assert.equal(result.remaining,transient?0:1)
 assert.equal(second,1)
 if(!transient)assert.ok(result.errors[0].includes('resource stuck'))
}
console.log('Structured close: transient/persistent failure and independent resources PASS')

if(source.includes('function prepareMusic(signal)')) {
 for(const mode of ['success','cancel','changed-config']) {
  const f=fixture('music'),runtime=await f.start().p,controller=new AbortController()
  const music=runtime.agent.tools.invoke('netease_music',{}, {signal:controller.signal})
  const completion=mode!=='success'?assert.rejects(music):music
  await f.entered.promise
  assert.equal(f.counts.play??0,0,'music must wait for required credentials')
  if(mode==='cancel')controller.abort()
  if(mode==='changed-config')f.setConfig({netease:{appId:'new-account',privateKey:'new-key'}})
  f.pause.resolve();await completion
  assert.equal(f.counts.save,mode==='success'?1:0)
  assert.equal(f.counts.play??0,mode==='success'?1:0)
  if(mode==='success'){
   await Promise.all([runtime.agent.tools.invoke('netease_music',{}, {signal:controller.signal}),runtime.agent.tools.invoke('netease_music',{}, {signal:controller.signal})])
   assert.equal(f.counts.sync,1,'cached matching credentials must not resync')
  }
  await f.owner.close()
 }
 console.log('On-demand music readiness/cancel/cache/new-config: 3 scenarios PASS')
}

for(const mode of ['partial','reject','hard-stop']) {
 const timers=[],logs=[],exits=[],release=deferred()
 const begin=source.indexOf('async function shutdown()')
 const code=source.slice(begin,source.indexOf('\n}',begin)+2)
 const ctx=vm.createContext({musicCredentials:{close:async()=>({ok:true})},agentInitialization:{close:()=>mode==='reject'?Promise.reject(Error('cleanup exception')):mode==='hard-stop'?release.promise:Promise.resolve({ok:false,remaining:1,errors:['CuaToolkit: kill denied']})},
 setTimeout:(fn,ms)=>(timers.push({fn,ms}),timers.length),clearTimeout(){},ttsEngine:null,cancelActiveTurn(){},log:message=>logs.push(message),
 stopListening:async()=>{},stopSpeaking(){},app:{exit:code=>exits.push(code)}})
 vm.runInContext(code,ctx)
 const stopping=ctx.shutdown()
 if(mode==='hard-stop'){
  assert.equal(timers[0].ms,5000);timers[0].fn();assert.deepEqual(exits,[0]);assert.ok(logs.some(l=>l.includes('hard-stop')))
  release.resolve({ok:false,remaining:1,errors:['still active']})
 }
 await stopping
 assert.ok(logs.some(l=>l.includes(mode==='reject'?'cleanup failed':'"ok":false')))
 assert.ok(exits.includes(0))
}
console.log('Production shutdown partial/rejected cleanup and 5s hard-stop: PASS')

// Reproducible before/after: same hung music dependency against the reviewed initializer.
{
 const { execFileSync }=await import('node:child_process')
 const prefix=source.includes('LX_MUSIC_TOOL')?'lx/':''
 const before=execFileSync('git',['show',`a94fcfe:${prefix}apps/shell/main.mjs`],{encoding:'utf8'}).replace(/\r\n/g,'\n')
 const start=before.indexOf('async function ensureAgent(signal)')
 const previousCode=before.slice(start,before.indexOf('\n}',start)+2).replaceAll('import(','loadModule(')
 const f=fixture('music',false,previousCode),task=f.start()
 let settled=false;task.p.then(()=>{settled=true},()=>{settled=true})
 await f.entered.promise
 assert.equal(settled,false);assert.equal(f.counts.agent,0)
 const rejected=assert.rejects(task.p);task.c.abort();await rejected
 f.pause.resolve();await tick();await f.owner.close()
 console.log('Historical a94fcfe hung-music baseline: blocked; current non-music initialization: unblocked')
}
