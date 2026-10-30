import assert from 'node:assert/strict'
import { Agent } from '../packages/agent/loop.mjs'
const deferred = () => { let resolve; const promise=new Promise(r=>resolve=r); return {promise,resolve} }
const calls=[{id:'1',name:'first',arguments:'{}'},{id:'2',name:'second',arguments:'{}'}]
let cases=0
for (const phase of ['model','gate','gate-return','tool','between','closing']) {
  const controller=new AbortController(), paused=deferred(), entered=deferred()
  let invoked=[], events=[], modelCalls=0, delta
  const agent=new Agent({ maxSteps:1,
    llm:{chat: async options=>{
      assert.equal(options.signal,controller.signal)
      delta=options.onDelta
      modelCalls++
      if (phase==='model' || (phase==='closing' && modelCalls===2)) {entered.resolve(); await paused.promise}
      return {text:'result',toolCalls:modelCalls===1?calls:[]}
    }},
    gate:async ({signal})=>{
      assert.equal(signal,controller.signal)
      if (phase==='gate') { entered.resolve(); await paused.promise }
      if (phase==='gate-return') controller.abort()
      return {allow:true}
    },
    tools:{listTools:()=>[],invoke:async name=>{
      invoked.push(name)
      if (phase==='tool') {entered.resolve();await paused.promise}
      return {text:'ok'}
    }},
    onEvent:e=>{
      events.push(e)
      if(phase==='between' && e.type==='tool_end') controller.abort()
    },
  })
  const pending=agent.run('test',{signal:controller.signal})
  const rejection=assert.rejects(pending,e=>e.name==='AbortError')
  if(['model','gate','tool','closing'].includes(phase)) {
    await entered.promise
    controller.abort()
    const before=events.length
    delta('late delta')
    assert.equal(events.length,before,'cancelled delta must be suppressed')
    paused.resolve()
  }
  await rejection
  assert.deepEqual(invoked, ['tool','between'].includes(phase)?['first']:phase==='closing'?['first','second']:[])
  assert.equal(events.some(e=>e.type==='done'),false)
  cases++
}
// Refusing one request does not cancel the turn; the next ordinary tool may run.
{
 let invoked=[];let n=0
 const agent=new Agent({llm:{chat:async()=>({text:'',toolCalls:n++===0?calls:[]})},
 tools:{listTools:()=>[],invoke:async name=>(invoked.push(name),{text:'ok'})},
 gate:async({tool})=>({allow:tool==='second'})})
 await agent.run('test')
 assert.deepEqual(invoked,['second']);cases++
}
console.log(`Agent cancellation: ${cases} scenarios PASS (mock LLM/tools, no system actions)`)
import { createServer } from 'node:http'
import { LLMClient } from '../packages/agent/llm.mjs'
// Real HTTP/SSE cancellation using a loopback server; no external model or tool.
{
  const entered=deferred(),closed=deferred(),controller=new AbortController()
  const server=createServer((_req,res)=>{
    res.writeHead(200,{'content-type':'text/event-stream'})
    res.write(': waiting\n\n')
    res.on('close',()=>closed.resolve())
    entered.resolve()
  })
  await new Promise(r=>server.listen(0,'127.0.0.1',r))
  try {
    const llm=new LLMClient({baseURL:`http://127.0.0.1:${server.address().port}/v1`,apiKey:'test',model:'test',timeoutMs:5000})
    let invoked=0
    const agent=new Agent({llm,tools:{listTools:()=>[],invoke:async()=>{invoked++}}})
    const pending=agent.run('test',{signal:controller.signal})
    const rejected=assert.rejects(pending)
    await entered.promise
    controller.abort()
    await rejected
    let timer
    await Promise.race([closed.promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('HTTP stream did not close after abort')),2000)})]).finally(()=>clearTimeout(timer))
    assert.equal(invoked,0)
    console.log('Loopback HTTP/SSE abort: PASS')
  } finally {
    server.closeAllConnections()
    await new Promise(r=>server.close(r))
  }
}
