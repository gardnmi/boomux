import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[],operations=[],inputs=[];
 page.on('pageerror',e=>errors.push(e.message));
 const workspaces=[0,1].map(i=>({id:`w${i}`,name:`Workspace ${i}`,default_cwd:'/tmp',agents:[],shells:[0,1].map(j=>({id:`s${i}${j}`,workspace_id:`w${i}`,name:`Shell ${i}${j}`,cwd:'/tmp',status:'running',run:{id:`r${i}${j}`}}))}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'w0',nodes:[],snapshot:{workspaces}}}));
 await page.route('**/api/attach',r=>r.fulfill({json:{token:'fixture'}}));
 await page.route('**/api/resource',r=>{const op=r.request().postDataJSON().operation;operations.push(op);if(op.action==='rename')workspaces.flatMap(w=>w.shells).find(s=>s.id===op.id).name=op.name;return r.fulfill({json:{ok:true}});});
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('\x1b[?1049h\x1b[?1000h\x1b[?1006hterminal with application mouse tracking'));ws.onMessage(m=>{if(typeof m!=='string')inputs.push(Buffer.from(m).toString());});});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);


 const pane=i=>page.locator('#panes .pane').nth(i),box=i=>pane(i).boundingBox();
 const menus=[];
 await page.exposeFunction('recordMenu',value=>menus.push(value));
 await page.evaluate(()=>window.addEventListener('contextmenu',e=>setTimeout(()=>window.recordMenu(e.defaultPrevented),0),true));
 // Reproduce modifier loss on subsequent mouse events while Ctrl remains held.
 await page.evaluate(()=>{let dropped=false;for(const kind of ['pointerdown','pointermove','pointerup','mousedown','mouseup','contextmenu'])window.addEventListener(kind,e=>{if(dropped)Object.defineProperty(e,'ctrlKey',{value:false});if(kind==='pointerup'&&e.ctrlKey)dropped=true;},true);});
 await page.keyboard.down('Control');
 for(const index of [0,1]){
  const before=await box(0),body=await pane(index).locator('.pane-body').boundingBox();
  await page.mouse.move(body.x+60,body.y+60);await page.mouse.down({button:'right'});await page.mouse.move(body.x+130,body.y+60,{steps:5});await page.mouse.up({button:'right'});await page.waitForTimeout(80);
  assert.ok((await box(0)).width>before.width+50,'divider follows pointer from either tile side');
 }
 const before=await box(0),body=await pane(0).locator('.pane-body').boundingBox();
 await page.mouse.move(body.x+60,body.y+60);await page.mouse.down({button:'right'});await page.mouse.move(body.x-20,body.y+60,{steps:5});await page.keyboard.press('Escape');await page.mouse.up({button:'right'});await page.waitForTimeout(80);
 assert.deepEqual(await box(0),before,'Escape restores tiled split');
 await page.keyboard.up('Control');
 await pane(0).getByRole('button',{name:'Toggle floating',exact:true}).click();await page.waitForTimeout(450);
 const floatBefore=await box(0),floatingBody=await pane(0).locator('.pane-body').boundingBox();
 await page.keyboard.down('Control');await page.mouse.move(floatingBody.x+60,floatingBody.y+60);await page.mouse.down({button:'right'});await page.mouse.move(floatingBody.x+140,floatingBody.y+120,{steps:5});await page.mouse.up({button:'right'});await page.keyboard.up('Control');await page.waitForTimeout(80);
 const after=await box(0);assert.equal(after.x,floatBefore.x);assert.equal(after.y,floatBefore.y);assert.ok(after.width>floatBefore.width+70&&after.height>floatBefore.height+50,'floating bottom-right grows on both axes');
 assert.ok(menus.length>0&&menus.every(Boolean),'resize suppresses native/terminal context menus');
 assert.ok(!inputs.some(input=>input.includes('\x1b[<')),'resize never sends application mouse input');
 await page.mouse.click(after.x+70,after.y+80,{button:'right'});await page.waitForTimeout(60);assert.equal(menus.at(-1),false,'ordinary right click remains available');
 assert.deepEqual(errors,[]);
 console.log('Ctrl-right resize: both tile sides, repeated gestures with dropped modifiers, floating dimensions, Escape, and context menus passed');
}finally{await browser.close();}
