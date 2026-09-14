import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:process.env.HEADED!=='1',args:['--no-sandbox','--enable-unsafe-webgpu']});
const gone=pid=>{try{process.kill(Number(pid),0);return false;}catch(e){return e.code==='ESRCH';}};
async function eventually(check,message){for(let i=0;i<60;i++){if(await check())return;await new Promise(r=>setTimeout(r,100));}assert.fail(message);}
try{
  const page=await browser.newPage({viewport:{width:1440,height:950}}),streams=new Map(),errors=[];
  page.on('pageerror',e=>errors.push(e.message));
  page.on('websocket',socket=>{
    const record={output:'',input:[],pid:null};
    socket.on('framereceived',({payload})=>{
      if(typeof payload==='string'){const message=JSON.parse(payload);if(message.type==='ready'){record.pid=String(message.pid);streams.set(record.pid,record);}}
      else record.output=(record.output+payload.toString()).slice(-131072);
    });
    socket.on('framesent',({payload})=>{if(payload.toString()==='not json')return;const m=JSON.parse(payload.toString());if(m.type==='input')record.input.push(m.data);});
  });
  await page.goto('http://127.0.0.1:4387');
  await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-pid]').length===4);
  const pane=page.locator('.pane[data-id="1"]'),body=pane.locator('.pane-body');
  const pid=await body.getAttribute('data-pid');await eventually(()=>streams.has(pid),'ready frame');const stream=streams.get(pid);
  await body.click({position:{x:50,y:40}});
  await page.keyboard.type("POC_VALUE=ghostty-live; printf '\\n%s\\n' \"$POC_VALUE\"");await page.keyboard.press('Enter');
  await eventually(()=>stream.output.includes('\r\nghostty-live\r\n'),'keyboard input must execute in the real shell');
  await page.keyboard.press('Control+Space');
  const inputCount=stream.input.length;
  await page.keyboard.press('Shift+ArrowRight');await page.waitForTimeout(250);
  await page.keyboard.press('Alt+ArrowLeft');await page.waitForTimeout(250);
  assert.equal(await body.getAttribute('data-pid'),pid,'tiling must preserve the same PTY process');
  assert.equal(stream.input.length,inputCount,'layout shortcuts must not leak into shell input');
  await page.keyboard.press('Escape');await page.waitForTimeout(250);
  await page.keyboard.type("printf '\\nkept:%s\\n' \"$POC_VALUE\"; stty size");await page.keyboard.press('Enter');
  await eventually(()=>stream.output.includes('\r\nkept:ghostty-live\r\n'),'shell variables survive movement');
  const rows=await body.getAttribute('data-rows'),cols=await body.getAttribute('data-cols');
  await eventually(()=>stream.output.includes(`\r\n${rows} ${cols}\r\n`),'PTY dimensions must match Ghostty dimensions');
  await page.keyboard.type('sleep 30');await page.keyboard.press('Enter');await page.waitForTimeout(150);await page.keyboard.press('Control+c');
  await page.keyboard.type("printf '\\ninterrupt-ok\\n'");await page.keyboard.press('Enter');
  await eventually(()=>stream.output.includes('\r\ninterrupt-ok\r\n'),'Ctrl+C interrupts the foreground job');
  await page.keyboard.type("printf '\\033[32mGhostty ANSI color works\\033[0m\\n'");await page.keyboard.press('Enter');
  await eventually(()=>stream.output.includes('\x1b[32mGhostty ANSI color works'),'ANSI output travels unchanged');
  if(process.env.SCREENSHOT)await page.screenshot({path:process.env.SCREENSHOT});
  await pane.getByRole('button',{name:'Close terminal session',exact:true}).click();await eventually(()=>gone(pid),'closing a pane must reap its shell');
  for(let cycle=0;cycle<3;cycle++){
    const old=await page.locator('.pane-body[data-pid]').evaluateAll(es=>es.map(e=>e.dataset.pid));
    await page.locator('#reset').click();await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-pid]').length===4);
    await eventually(()=>old.every(gone),'reset must reclaim every old shell');
  }
  for(const mode of ['malformed','backlog']){
    const result=await page.evaluate(mode=>new Promise((resolve,reject)=>{
      const ws=new WebSocket(`ws://${location.host}/pty?cols=80&rows=24`);let pid;
      const timer=setTimeout(()=>{ws.close();reject(Error('Overload test timed out'));},6000);
      ws.onmessage=e=>{
        if(typeof e.data!=='string')return;
        const message=JSON.parse(e.data);if(message.type!=='ready')return;pid=message.pid;
        ws.send(mode==='malformed'?'not json':JSON.stringify({type:'input',data:"printf '%1048577s' x\r"}));
      };
      ws.onclose=e=>{clearTimeout(timer);resolve({pid,code:e.code,reason:e.reason});};
    }),mode);
    assert.ok(result.pid);assert.equal(result.code,1008);
    assert.match(result.reason,mode==='malformed'?/Invalid/:/backlog/);
    await eventually(()=>gone(result.pid),'overload and malformed input must reclaim the PTY');
  }
  const remaining=await page.locator('.pane-body[data-pid]').evaluateAll(es=>es.map(e=>e.dataset.pid));
  await page.close();await eventually(()=>remaining.every(gone),'closing the page must reclaim every shell');
  assert.deepEqual(errors,[]);
  for(const origin of ['https://example.com','null']){
    const response=await fetch('http://127.0.0.1:4387/pty?cols=80&rows=24',{headers:{Origin:origin}});assert.equal(response.status,403);
  }
  assert.equal((await fetch('http://127.0.0.1:4387/pty?cols=99999&rows=24',{headers:{Origin:'http://127.0.0.1:4387'}})).status,400);
  console.log('Ghostty input/output, ANSI, PTY identity, resize, layout isolation, Ctrl+C, repeated cleanup, overload, and origin checks passed');
}finally{await browser.close();}
