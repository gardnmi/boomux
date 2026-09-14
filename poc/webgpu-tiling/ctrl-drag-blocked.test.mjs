import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],operations=[],inputs=[];
 page.on('pageerror',e=>errors.push(e.message));
 // Replay the real browser trace: keydown stays held, but mouse modifier flags
 // disappear after the first drag while focus moves to another terminal.
 await page.addInitScript(()=>{
  let dropped=false;
  for(const kind of ['pointerdown','pointermove','pointerup','mousedown','mouseup'])window.addEventListener(kind,e=>{
   if(dropped)Object.defineProperty(e,'ctrlKey',{value:false});
   if(kind==='pointerup'&&e.ctrlKey)dropped=true;
  },true);
 });
 const workspaces=[0,1].map(i=>({id:`w${i}`,name:`Workspace ${i}`,default_cwd:'/tmp',agents:[],shells:[0,1].map(j=>({id:`s${i}${j}`,workspace_id:`w${i}`,name:`Shell ${i}${j}`,cwd:'/tmp',status:'running',run:{id:`r${i}${j}`}}))}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'w0',nodes:[],snapshot:{workspaces}}}));
 await page.route('**/api/attach',r=>r.fulfill({status:409,json:{error:'shell already has an active controller; use takeover'}}));
 await page.route('**/api/resource',r=>{const op=r.request().postDataJSON().operation;operations.push(op);if(op.action==='rename')workspaces.flatMap(w=>w.shells).find(s=>s.id===op.id).name=op.name;return r.fulfill({json:{ok:true}});});
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('\x1b[?1049h\x1b[?1000h\x1b[?1006hterminal with application mouse tracking'));ws.onMessage(m=>{if(typeof m!=='string')inputs.push(Buffer.from(m).toString());});});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 await page.waitForFunction(()=>document.querySelectorAll('.attachment-error').length===2);
 await page.evaluate(()=>document.body.classList.add('hide-headings'));

 const message=page.locator('.attachment-error p').first();
 await message.dblclick();
 assert.equal(await page.evaluate(()=>getSelection().toString()),'', 'controller status must not become a native editable selection');
 assert.equal(await message.evaluate(el=>el.isContentEditable),false);
 await page.keyboard.down('Control');
 for(const surface of ['.pane-body','.attachment-error p'])for(const index of [0,1,0,1]){
  const pane=page.locator('#panes .pane').nth(index),body=await pane.locator(surface).boundingBox();
  await page.mouse.move(body.x+60,body.y+12);await page.mouse.down();await page.mouse.move(body.x+90,body.y+42,{steps:4});
  assert.ok(await pane.evaluate(el=>el.classList.contains('dragging')),`pane ${index} lifts with Ctrl still held`);
  await page.mouse.up();await page.waitForTimeout(30);
 }
 // Cancelling while the mouse is still down must release capture as well.
 const first=page.locator('#panes .pane').first(),rect=await first.locator('.pane-body').boundingBox();
 await page.mouse.move(rect.x+60,rect.y+20);await page.mouse.down();await page.mouse.move(rect.x+90,rect.y+50);
 await page.keyboard.press('Escape');
 assert.equal(await page.locator('.pane.dragging').count(),0);
 assert.equal(await first.evaluate(el=>el.hasPointerCapture(1)),false);
 await page.mouse.up();
 await page.keyboard.up('Control');
 async function ordinaryClick(){const r=await first.locator('.pane-body').boundingBox();await page.mouse.move(r.x+60,r.y+20);await page.mouse.down();await page.mouse.move(r.x+90,r.y+50);assert.equal(await page.locator('.pane.dragging').count(),0,'released Ctrl must not remain latched');await page.mouse.up();}
 await ordinaryClick();
 await page.keyboard.down('Control');
 // Leaving the browser must clear held keys even when its key-up is missed.
 await page.evaluate(()=>window.dispatchEvent(new Event('blur')));
 await ordinaryClick();await page.keyboard.up('Control');
 assert.deepEqual(errors,[]);assert.ok(!inputs.some(input=>input.includes('\x1b[<')),'Ctrl-drag must not send application mouse clicks');console.log('Repeated Ctrl drags across non-editable takeover overlays passed');
}finally{await browser.close();}
