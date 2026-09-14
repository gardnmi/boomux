// Synthetic terminal output: never attach to or type into a real Shell.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox']});
try{
  const page=await browser.newPage({viewport:{width:1440,height:950},deviceScaleFactor:Number(process.env.POC_DPR||1)}),resizes=[],errors=[];
  page.on('pageerror',e=>errors.push(e.message));
  await page.route('**/api/snapshot',r=>r.fulfill({status:404,body:''}));
  if(process.env.POC_APP_SOURCE)await page.route('**/app.js',async r=>r.fulfill({contentType:'text/javascript',body:await readFile(process.env.POC_APP_SOURCE,'utf8')}));
  await page.routeWebSocket('**/pty?*',ws=>{
    ws.onMessage(message=>{const m=JSON.parse(String(message));if(m.type==='resize')resizes.push(m);});
    ws.send(JSON.stringify({type:'ready',pid:1}));
    // Long wrapped lines fill the configured 2,000-line scrollback in each pane.
    ws.send(Buffer.from(Array.from({length:2000},(_,i)=>`${i} ${'scrollback fixture '.repeat(10)}\r\n`).join('')));
  });
  await page.goto(`${process.env.POC_URL||'http://127.0.0.1:4389'}/?fallback`);
  await page.waitForFunction(()=>document.querySelectorAll('.pane-body[data-connected="true"]').length===4);
  await page.waitForTimeout(800);
  await page.evaluate(()=>{window.resizeLongTasks=[];new PerformanceObserver(list=>window.resizeLongTasks.push(...list.getEntries().map(e=>e.duration))).observe({type:'longtask'});});
  const divider=page.locator('.divider.x').first(),original=await divider.boundingBox();
  const start=resizes.length;
  await page.mouse.move(original.x+4,original.y+65);await page.mouse.down();
  await page.mouse.move(original.x+124,original.y+65);await page.waitForTimeout(50);
  const moved=await divider.boundingBox();
  if(!process.env.POC_APP_SOURCE)assert.ok(Math.abs(moved.x-original.x-120)<2,'divider hit target follows the live split');
  const began=performance.now();
  for(let i=0;i<40;i++)await page.mouse.move(original.x+(i%2?140:-110),original.y+65);
  await page.waitForTimeout(120);
  const during=resizes.length-start;
  if(!process.env.POC_APP_SOURCE)assert.equal(during,0,'no scrollback reflow/PTY resize during drag');
  await page.mouse.up();await page.waitForTimeout(600);
  const after=resizes.length-start;
  if(!process.env.POC_APP_SOURCE)assert.ok(after>0&&after<=4,'one final grid update per affected terminal');
  // Cancellation restores geometry and must not resize the terminal grid.
  const settled=await divider.boundingBox(),cancelStart=resizes.length;
  await page.mouse.move(settled.x+4,settled.y+65);await page.mouse.down();await page.mouse.move(settled.x-80,settled.y+65);await page.keyboard.press('Escape');await page.mouse.up();await page.waitForTimeout(600);
  if(!process.env.POC_APP_SOURCE){assert.ok(Math.abs((await divider.boundingBox()).x-settled.x)<1);assert.equal(resizes.length,cancelStart);}
  assert.deepEqual(errors,[]);
  console.log(JSON.stringify({resizeMessagesDuringDrag:during,resizeMessagesIncludingCommit:after,elapsedMs:Math.round(performance.now()-began),longTasks:await page.evaluate(()=>window.resizeLongTasks)}));
}finally{await browser.close();}
