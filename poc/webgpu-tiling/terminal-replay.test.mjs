// Deliberately split reconstruction over frames: partial history must not paint.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage(),messages=[];let stream,grant;
 const workspace={id:'w',name:'workspace',agents:[],shells:[{id:'s',name:'shell',cwd:'/project',status:'running',run:{id:'r'}}]};
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'w',nodes:[],snapshot:{workspaces:[workspace]}}}));
 await page.route('**/api/attach',r=>{grant=r.request().postDataJSON();return r.fulfill({json:{token:'fixture'}});});
 await page.routeWebSocket('**/pty',ws=>{stream=ws;ws.onMessage(m=>{if(typeof m==='string')messages.push(JSON.parse(m));});});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 while(!stream)await page.waitForTimeout(10);
 const first=Buffer.from('\x1b[41m\x1b[2Jpartial history');
 const last=Buffer.from('\x1b[0m\x1b[2J\x1b[Hrestored screen');
 const screenshot=()=>page.locator('.pane-body canvas').evaluate(c=>c.toDataURL());
 await page.waitForTimeout(100);const before=await screenshot();
 stream.send(JSON.stringify({type:'attached',rows:grant.rows,cols:grant.cols,replay_bytes:first.length+last.length}));
 stream.send(first);await page.waitForTimeout(150);
 assert.equal(await screenshot(),before,'partial reconstruction never reaches the canvas');
 assert.equal(await page.locator('.pane-body').getAttribute('data-replaying'),'true');
 stream.send(last);await page.waitForTimeout(100);
 assert.notEqual(await screenshot(),before,'completed reconstruction paints');
 assert.equal(await page.locator('.pane-body').getAttribute('data-replaying'),'false');
 assert.equal(messages.filter(m=>m.type==='resize').length,0,'attachment does not send redundant resize/redraw requests');
 stream.send(JSON.stringify({type:'resize',cols:42,rows:12}));await page.waitForTimeout(100);
 assert.equal(await page.locator('.pane-body').getAttribute('data-cols'),'42','authoritative daemon resize is applied');
 assert.equal(messages.filter(m=>m.type==='resize').length,0,'server resize is not echoed');
 console.log('Chunked replay stays off-screen until complete; authoritative resize applies without echo');
}finally{await browser.close();}
