// Workspace reordering is a sidebar-only operation: no real daemon traffic.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1280,height:900}}),errors=[];let attachments=0;
 page.on('pageerror',e=>errors.push(e.message));
 const workspaces=Array.from({length:12},(_,i)=>({id:`w${i}`,name:`Workspace ${String(i).padStart(2,'0')}`,agents:[],shells:[{id:`s${i}`,workspace_id:`w${i}`,name:'shell',cwd:'/tmp',status:'running',run:{id:`r${i}`}}]}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'fixture',workspace_id:'w0',nodes:[],snapshot:{workspaces}}}));
 await page.route('**/api/changes',r=>r.fulfill({status:404}));
 await page.route('**/api/attach',r=>{attachments++;return r.fulfill({json:{token:'fixture'}});});
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('retained terminal'));});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4390')+'/?fallback');
 await page.waitForSelector('.pane-body[data-connected="true"]');await page.mouse.move(1100,10);
 const order=()=>page.locator('#workspace-list > .workspace-group').evaluateAll(els=>els.map(el=>el.dataset.workspaceId));
 const row=id=>page.locator(`[data-workspace-id="${id}"] .workspace-button`);
 async function dragTo(source,target){const a=await row(source).boundingBox(),b=await row(target).boundingBox();await page.mouse.move(a.x+60,a.y+a.height/2);await page.mouse.down();await page.mouse.move(b.x+60,b.y+b.height-5,{steps:5});await page.waitForTimeout(60);}
 await page.evaluate(()=>window.reorderCanvas=document.querySelector('.pane-body canvas'));
 await dragTo('w0','w2');
 assert.equal(await page.locator('.workspace-drag-ghost').count(),1,'lifted card follows drag');
 assert.equal(await page.locator('.workspace-drag-source').count(),1,'landing slot is visible');
 assert.notEqual((await order())[0],'w0','order changes before releasing the pointer');
 assert.ok(await page.locator('#workspace-list').evaluate(el=>el.getAnimations({subtree:true}).length>0),'neighbors animate during drag');
 await page.screenshot({path:'/tmp/boomux-workspace-reorder-during.png'});
 await page.mouse.up();await page.waitForTimeout(400);
 const committed=await order();assert.equal(committed[2],'w0');assert.equal(await page.locator('.workspace-group.current').getAttribute('data-workspace-id'),'w0','drag does not switch workspace');
 assert.equal(await page.locator('.workspace-drag-ghost').count(),0);
 assert.deepEqual(await page.evaluate(()=>JSON.parse(localStorage.getItem('boomux.web.order'))),committed);
 assert.equal(attachments,1);assert.ok(await page.evaluate(()=>window.reorderCanvas===document.querySelector('.pane-body canvas')));
 // Escape restores the original order and cleans the lifted card.
 await dragTo('w1','w3');await page.keyboard.press('Escape');await page.mouse.up();await page.waitForTimeout(400);
 assert.deepEqual(await order(),committed);assert.equal(await page.locator('.workspace-drag-ghost').count(),0);
 // Holding near the bottom scrolls without more pointer events.
 const a=await row('w1').boundingBox(),list=await page.locator('#workspace-list').boundingBox();
 await page.mouse.move(a.x+50,a.y+25);await page.mouse.down();await page.mouse.move(list.x+80,list.y+list.height-8,{steps:6});await page.waitForTimeout(550);
 assert.ok(await page.locator('#workspace-list').evaluate(el=>el.scrollTop>50),'edge auto-scroll');await page.keyboard.press('Escape');await page.mouse.up();
 await page.locator('#workspace-list').evaluate(el=>el.scrollTop=0);await page.waitForTimeout(400);
 // Reduced motion keeps live destination feedback but skips the tween.
 await page.emulateMedia({reducedMotion:'reduce'});await dragTo('w1','w3');assert.equal(await page.locator('#workspace-list').evaluate(el=>el.getAnimations({subtree:true}).length),0);await page.mouse.up();
 const beforeKey=await order();await row('w1').focus();await page.keyboard.press('Alt+ArrowUp');const afterKey=await order();assert.equal(afterKey.indexOf('w1'),beforeKey.indexOf('w1')-1);
 assert.deepEqual(errors,[]);console.log('Live animated reorder, landing slot, persistence, cancellation, auto-scroll, reduced motion, keyboard alternative and terminal identity passed');
}finally{await browser.close();}
