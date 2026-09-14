// Mock attachments: workspace switches must attach using the final pane grid.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 const page=await browser.newPage({viewport:{width:1440,height:950}}),requests=[],errors=[];
 page.on('pageerror',e=>errors.push(e.message));
 const workspaces=[2,4,1,1].map((count,i)=>({id:`w${i}`,name:`Workspace ${i}`,agents:[],shells:Array.from({length:count},(_,j)=>({id:`s${i}-${j}`,name:`Shell ${i}-${j}`,cwd:'/project',status:'running',run:{id:`r${i}-${j}`}}))}));
 await page.route('**/api/snapshot',r=>r.fulfill({json:{node_id:'local',workspace_id:'w0',nodes:[],snapshot:{workspaces}}}));
 await page.route('**/api/attach',r=>{requests.push(r.request().postDataJSON());return r.fulfill({json:{token:'fixture'}});});
 await page.routeWebSocket('**/pty',ws=>{ws.send(JSON.stringify({type:'attached'}));ws.send(Buffer.from('restored prompt\r\n'));});
 await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
 for(const index of [0,1,0,1]){
  if(index!==0||requests.length){
   const current=await page.locator('.workspace-group.current').getAttribute('data-workspace-id');
   await page.locator(`[data-workspace-id="w${index}"] .workspace-button`).click();
   if(current!==`w${index}`){
    const animation=await page.locator('#panes').evaluate(el=>{
     const a=el.getAnimations()[0];return a?{duration:a.effect.getTiming().duration,start:a.effect.getKeyframes()[0].transform}:null;
    });
    assert.equal(animation?.duration,360,'Desktop smooth transition duration');
    assert.equal(animation.start.includes('(-'),index===0,'slide follows sidebar order');
   }
  }
  await page.waitForFunction(count=>document.querySelectorAll('.pane-body[data-connected="true"]').length===count,workspaces[index].shells.length);
  await page.waitForTimeout(450);
  await page.evaluate(index=>{
    window.retainedCanvases ||= new Map();
    const canvas=document.querySelector('.pane-body canvas');
    const previous=window.retainedCanvases.get(index);
    if(previous&&previous!==canvas)throw Error('Workspace switch replaced its terminal canvas');
    window.retainedCanvases.set(index,canvas);
  },index);
  const sizes=await page.locator('.pane-body').evaluateAll(els=>els.map(el=>({id:el.dataset.shellId,cols:Number(el.dataset.cols),rows:Number(el.dataset.rows)})));
  for(const size of sizes){const request=requests.findLast(r=>r.shell_id===size.id);assert.equal(request.cols,size.cols,`${size.id} attaches at final columns`);assert.equal(request.rows,size.rows,`${size.id} attaches at final rows`);}
 }
 assert.equal(requests.length,6,'returning to recent workspaces does not reconnect any terminals');
 for(const index of [2,3,0]){
  await page.locator(`[data-workspace-id="w${index}"] .workspace-button`).click();
  await page.waitForFunction(count=>document.querySelectorAll('.pane-body[data-connected="true"]').length===count,workspaces[index].shells.length);
 }
 assert.equal(requests.length,10,'oldest workspace is evicted when the inactive cache is full');
 // Rapid switches cancel old presentation layers without touching live views.
 for(const index of [1,0,1])await page.locator(`[data-workspace-id="w${index}"] .workspace-button`).click();
 await page.waitForTimeout(450);
 assert.equal(await page.locator('.workspace-outgoing').count(),0,'interrupted slides release outgoing layers');
 assert.equal(await page.locator('#panes').evaluate(el=>el.getAnimations().length),0);
 await page.emulateMedia({reducedMotion:'reduce'});
 await page.locator('[data-workspace-id="w0"] .workspace-button').click();
 assert.equal(await page.locator('.workspace-outgoing').count(),0,'reduced motion switches instantly');
 assert.deepEqual(errors,[]);
 console.log('Workspace switches preserve canvas identity and connections; bounded cache evicts oldest views');
}finally{await browser.close();}
