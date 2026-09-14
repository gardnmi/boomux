import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),errors=[];
 page.on('pageerror',e=>errors.push(e.message));
 const workspaces=[0,1].map(i=>({id:`w${i}`,name:`Workspace ${i}`,agents:[],shells:[0,1].map(j=>({id:`s${i}${j}`,workspace_id:`w${i}`,name:`Shell ${i}${j}`,cwd:'/tmp',status:'running',run:{id:`r${i}${j}`}}))}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'w0',nodes:[],snapshot:{workspaces}}}));
 await page.route('**/api/changes',r=>r.fulfill({status:404}));
 await page.route('**/api/attach',r=>r.fulfill({json:{token:'fixture'}}));
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('same session'));});
 const ready=count=>page.waitForFunction(n=>document.querySelectorAll('#panes .pane-body[data-connected="true"]').length===n,count);
 const pref=async(key,value)=>{await page.locator('#settings').evaluate(el=>el.open=true);await page.locator(`[data-preference="${key}"][data-value="${value}"]`).click();await page.getByRole("button",{name:"Close Settings",exact:true}).click();};
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');await ready(2);
 assert.ok(await page.evaluate(()=>document.fonts.check('13px "Boomux Terminal"')),'bundled terminal font loaded');
 await pref('scope','mixed');await page.locator('[data-workspace-id="w1"] .workspace-button').click();await ready(4);
 await page.reload();await ready(4);assert.equal(await page.locator('[data-preference="scope"][data-value="mixed"]').getAttribute('aria-pressed'),'true','Mixed scope and exact Shell layouts survive reload');
 await pref('scope','workspace');await ready(2);assert.deepEqual(await page.locator('#panes .pane-body').evaluateAll(els=>els.map(el=>el.dataset.shellId).sort()),['s10','s11']);
 await pref('motion','instant');
 await page.locator('#panes .pane [data-action="minimize"]').last().click();await ready(1);
 await page.reload();await ready(1);assert.equal(await page.getByTitle('Restore Shell',{exact:true}).count(),1,'minimized strip persists');
 await page.locator('[data-workspace-id="w0"] .workspace-button').click();await ready(2);await page.locator('[data-workspace-id="w1"] .workspace-button').click();await ready(1);
 await page.reload();await ready(1);assert.equal(await page.getByTitle('Restore Shell',{exact:true}).count(),1);
 await page.getByTitle('Restore Shell',{exact:true}).click();await ready(2);
 assert.deepEqual(errors,[]);console.log('Mixed scope, scoped cleanup, bundled font, per-workspace layouts and minimized persistence passed');
}finally{await browser.close();}
