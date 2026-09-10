// This fixture mutates only its newly created Shells, and may restart the
// explicitly selected isolated daemon. Never run against the ordinary Node.
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
const exec=promisify(execFile);
const cli=process.env.POC_BOOMUX_BIN;
assert.ok(cli&&process.env.BOOMUX_RUNTIME_DIR?.endsWith('/target/webgpu-poc/runtime'),'Select the isolated PoC daemon and POC_BOOMUX_BIN');
const base=process.env.POC_URL||'http://127.0.0.1:4389';
const info=await (await fetch(`${base}/api/snapshot`)).json();
const local=JSON.parse((await exec(cli,['node','snapshot','--json'])).stdout).data.nodes.find(n=>n.local);
assert.equal(local.node_id,info.node_id,'Gateway and test CLI must identify the same isolated Node');
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:process.env.HEADED!=='1',args:['--no-sandbox','--enable-unsafe-webgpu']});
const created=[];
const snapshot=async()=>await (await fetch(`${base}/api/snapshot`)).json();
async function eventually(check,message){for(let i=0;i<100;i++){if(await check())return;await new Promise(r=>setTimeout(r,100));}assert.fail(message);}
try{
  const context=await browser.newContext({viewport:{width:1440,height:950}});
  // Start with no open panes so the fixture never types into pre-existing work.
  await context.addInitScript(({node,workspace})=>{
    if(!localStorage.getItem('boomux.webgpu.layout.v1'))localStorage.setItem('boomux.webgpu.layout.v1',JSON.stringify({version:1,node_id:node,workspace_id:workspace,panes:[],tree:null,floating:[],active:null}));
  },{node:info.node_id,workspace:info.workspace_id});
  const page=await context.newPage();const streams=[],errors=[];
  page.on('pageerror',e=>errors.push(e.message));
  page.on('websocket',ws=>{const stream={output:'',messages:[],input:[]};streams.push(stream);
    ws.on('framereceived',({payload})=>{if(typeof payload==='string')stream.messages.push(JSON.parse(payload));else stream.output=(stream.output+payload.toString()).slice(-200000);});
    ws.on('framesent',({payload})=>{if(typeof payload!=='string')stream.input.push(payload.toString());});
  });
  await page.goto(base);await page.waitForSelector('#workspace-list');
  assert.equal(await page.locator('.pane').count(),0);
  await page.locator('#add').click();await page.waitForSelector('.pane-body[data-connected="true"]');
  let body=page.locator('.pane-body').first();const shell=await body.getAttribute('data-shell-id'),run=await body.getAttribute('data-run-id');created.push(shell);
  await body.click({position:{x:30,y:30}});
  await page.keyboard.type("WEB_POC=survives; printf '\\nBEFORE:%s:%s\\n' \"$$\" \"$WEB_POC\"");await page.keyboard.press('Enter');
  await eventually(()=>streams.some(s=>/BEFORE:\d+:survives\r\n/.test(s.output)),'initial PTY command');
  const pid=streams.map(s=>s.output).join('').match(/BEFORE:(\d+):survives\r\n/)[1];
  await page.locator('#add').click();await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);
  created.push(await page.locator('.pane-body').nth(1).getAttribute('data-shell-id'));
  await page.mouse.move(10,10);await page.locator('.pane').first().locator('.pane-heading').click({position:{x:50,y:20}});
  await page.keyboard.press('Control+Space');await page.keyboard.press('Alt+ArrowRight');await page.waitForTimeout(250);await page.keyboard.press('Escape');await page.waitForTimeout(250);
  const geometry=await page.locator('.pane').first().boundingBox();
  const total=(await snapshot()).snapshot.workspaces.flatMap(w=>w.shells).length;
  await page.reload();await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);await page.waitForTimeout(250);
  assert.deepEqual(await page.locator('.pane').first().boundingBox(),geometry,'saved split geometry survives refresh');
  body=page.locator('.pane-body').first();assert.equal(await body.getAttribute('data-shell-id'),shell);assert.equal(await body.getAttribute('data-run-id'),run);
  assert.equal((await snapshot()).snapshot.workspaces.flatMap(w=>w.shells).length,total,'refresh creates no replacement Shells');
  await body.click({position:{x:30,y:30}});await page.keyboard.type("printf '\\nAFTER:%s:%s\\n' \"$$\" \"$WEB_POC\"; stty size");await page.keyboard.press('Enter');
  await eventually(()=>streams.some(s=>s.output.includes(`AFTER:${pid}:survives\r\n`)),'same shell variables and PID after refresh');
  const rows=await body.getAttribute('data-rows'),cols=await body.getAttribute('data-cols');
  await eventually(()=>streams.some(s=>s.output.includes(`\r\n${rows} ${cols}\r\n`)),'browser controls PTY size');
  const peer=await context.newPage();await peer.goto(base);await peer.waitForSelector('.attachment-error');
  assert.equal(await peer.getByRole('button',{name:'Take control',exact:true}).count(),2);
  peer.once('dialog',dialog=>dialog.accept());await peer.getByRole('button',{name:'Take control',exact:true}).first().click();
  await peer.waitForSelector('.pane-body[data-connected="true"]');
  await eventually(()=>page.locator('.pane-body').first().getAttribute('data-connected').then(v=>v==='false'),'explicit takeover detaches previous controller');
  await peer.close();await page.reload();await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);
  if(process.env.POC_RESTART==='1'){
    await exec(cli,['daemon','restart','--executable',cli]);
    await eventually(()=>streams.some(s=>s.messages.some(m=>m.type==='reconnecting')),'daemon reconnect handshake');
    await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);
    body=page.locator('.pane-body').first();await body.click({position:{x:30,y:30}});
    await page.keyboard.type("printf '\\nHANDOFF:%s:%s\\n' \"$$\" \"$WEB_POC\"");await page.keyboard.press('Enter');
    await eventually(()=>streams.some(s=>s.output.includes(`HANDOFF:${pid}:survives\r\n`)),'handoff preserves PID, state, and input');
  }
  await page.locator('.pane').first().getByRole('button',{name:'Detach pane',exact:true}).click();
  assert.ok((await snapshot()).snapshot.workspaces.flatMap(w=>w.shells).some(s=>s.id===shell&&s.run?.id===run&&s.status==='running'),'pane closure detaches without terminating');
  await page.reload();await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===1);
  assert.equal(await page.locator('.pane').count(),1,'detached pane stays detached after refresh');
  await page.close();assert.ok((await snapshot()).snapshot.workspaces.flatMap(w=>w.shells).some(s=>s.id===shell&&s.run?.id===run));
  const rejected=await fetch(`${base}/api/attach`,{method:'POST',headers:{Origin:base,'Content-Type':'application/json'},body:JSON.stringify({node_id:info.node_id,shell_id:shell,run_id:'stale-run',rows:24,cols:80})});assert.equal(rejected.status,409);
  assert.equal((await fetch(`${base}/api/shell`,{method:'POST',headers:{Origin:'https://example.com','Content-Type':'application/json'},body:'{}'})).status,403);
  assert.deepEqual(errors,[]);
  console.log('Daemon creation, primary resize, refresh/layout identity, detach, explicit takeover, stale-run/origin rejection'+(process.env.POC_RESTART==='1'?', and graceful handoff':'')+' passed');
}finally{
  await browser.close();
  for(const id of created)await exec(cli,['shell','close',id]);
}
