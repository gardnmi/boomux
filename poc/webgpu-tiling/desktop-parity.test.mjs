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
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('terminal ready'));ws.onMessage(m=>{if(typeof m!=='string')inputs.push(Buffer.from(m).toString());});});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===2);
 await page.keyboard.press('F1');await page.locator('#shortcuts-dialog').waitFor();await page.keyboard.press('Escape');
 await page.keyboard.press('F6');assert.equal(await page.evaluate(()=>document.activeElement.className),'workspace-button');await page.keyboard.press('ArrowDown');assert.equal(await page.evaluate(()=>document.activeElement.className),'shell-button');
 await page.keyboard.press('F6');await page.keyboard.press('F2');await page.locator('.resource-dialog input').fill('renamed');await page.getByRole('button',{name:'Rename Shell',exact:true}).last().click();await page.waitForTimeout(100);assert.equal(operations[0].action,'rename');assert.ok(await page.locator('.pane-name').filter({hasText:'renamed'}).count());
 await page.keyboard.press('Control+Space');assert.equal(await page.locator('#layout-mode').getAttribute('aria-pressed'),'true');
 await page.keyboard.press('Control+PageDown');await page.waitForTimeout(450);assert.equal(await page.locator('.workspace-group.current').getAttribute('data-workspace-id'),'w1');
 await page.keyboard.press('b');assert.ok(await page.locator('body').evaluate(el=>el.classList.contains('sidebar-hidden')));await page.keyboard.press('b');
 await page.keyboard.press('o');await page.waitForTimeout(450);await page.keyboard.press('c');await page.waitForTimeout(450);
 const floating=page.locator('#panes .pane').last();const before=await floating.boundingBox();const handle=await floating.locator('.pane-resize.se').boundingBox();await page.mouse.move(handle.x+4,handle.y+4);await page.mouse.down();await page.mouse.move(handle.x+70,handle.y+40);await page.mouse.up();await page.waitForTimeout(500);assert.ok((await floating.boundingBox()).width>before.width);
 await page.keyboard.press('Escape');await page.waitForTimeout(550);
 await page.keyboard.down('Control');await page.keyboard.down('Space');await page.waitForTimeout(300);await page.keyboard.up('Space');await page.keyboard.up('Control');assert.equal(await page.locator('#layout-mode').getAttribute('aria-pressed'),'false','held leader releases temporary Layout');
 await page.waitForTimeout(550);await page.keyboard.press('Control+Space');await page.keyboard.press('Control+Space');await page.waitForTimeout(100);assert.ok(inputs.includes('\0'),'double tap forwards Ctrl+Space');
 await page.getByLabel('Settings',{exact:true}).click();await page.locator('[data-preference=layout][data-value=tabs]').click();await page.waitForTimeout(500);assert.equal(await page.locator('#panes .pane:visible').count(),1);assert.equal(await page.locator('#pane-tabs button').count(),2);
 await page.locator('#panes .pane:visible [data-action="minimize"]').click();await page.waitForTimeout(450);assert.ok(await page.getByTitle('Restore Shell',{exact:true}).count());await page.getByTitle('Restore Shell',{exact:true}).click();await page.waitForTimeout(450);
 assert.deepEqual(errors,[]);await page.screenshot({path:'/tmp/boomux-desktop-parity.png'});
 console.log('Desktop help/sidebar keys, rename, leader hold/pass-through, workspace keys, floating corner resize, Tabs, minimize/restore passed');
}finally{await browser.close();}
