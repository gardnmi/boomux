// Intercept every attachment: never manipulate real Shells.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[];let connections=0,closed=0;
 page.on('pageerror',e=>errors.push(e.message));
 await page.route('**/api/snapshot',r=>r.fulfill({status:404,body:''}));
 await page.routeWebSocket('**/pty?*',ws=>{connections++;ws.onClose(()=>closed++);ws.send(JSON.stringify({type:'ready',pid:1}));ws.send(Buffer.from('retained output\r\n$ '));});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===4);
 const pane=page.locator('.pane').first();
 await pane.getByRole('button',{name:'Minimize pane',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('.pane').style.display==='none');
 assert.equal(await page.locator('.sidebar-pane').first().locator('small').textContent(),'minimized');
 await page.locator('.sidebar-pane').first().click();
 await pane.waitFor({state:'visible'});assert.equal(connections,4);assert.equal(closed,0,'standalone PTY remains connected');
 for(let i=0;i<4;i++){
  await page.locator('.pane:visible').first().getByRole('button',{name:'Minimize pane',exact:true}).click();
  await page.waitForFunction(n=>[...document.querySelectorAll('.pane')].filter(p=>getComputedStyle(p).display!=='none').length===n,3-i);
 }
 assert.equal(await page.locator('#empty').isVisible(),true);
 await page.keyboard.press('Control+Space');await page.keyboard.press('Tab');await page.keyboard.press('Escape');
 await page.locator('.sidebar-pane').first().click();await pane.waitFor({state:'visible'});
 assert.equal(connections,4);assert.equal(closed,0);
 await page.close();
 const daemon=await browser.newPage({viewport:{width:1440,height:950}}),requests=[];let detached=0;
 daemon.on('pageerror',e=>errors.push(e.message));
 const shell={id:'shell',name:'build',cwd:'/project',status:'running',run:{id:'run'}};
 await daemon.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'workspace',nodes:[],snapshot:{workspaces:[{id:'workspace',name:'project',shells:[shell],agents:[]}]}}}));
 await daemon.route('**/api/attach',r=>{requests.push(r.request().postDataJSON());return r.fulfill({json:{token:'fixture'}});});
 await daemon.routeWebSocket('**/pty',ws=>{ws.onClose(()=>detached++);ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('persistent Shell\r\n'));});
 await daemon.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 await daemon.waitForSelector('.pane-body[data-connected="true"]');
 await daemon.getByRole('button',{name:'Minimize pane',exact:true}).click();
 assert.equal(await daemon.locator('.pane').count(),0);assert.equal(detached,1);
 assert.ok((await daemon.locator('.shell-button').textContent()).includes('running'));
 await daemon.locator('.shell-button').click();await daemon.waitForSelector('.pane-body[data-connected="true"]');
 assert.equal(requests.length,2);assert.equal(requests[0].shell_id,requests[1].shell_id);assert.equal(requests[0].run_id,requests[1].run_id);
 assert.equal(requests[1].takeover,false,'restore uses normal attachment policy');
 assert.deepEqual(errors,[]);console.log('Minimize/restore, all-minimized navigation, retained standalone PTYs, and daemon detach/reattach to the same ShellRun passed');
}finally{await browser.close();}
