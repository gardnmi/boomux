// Regression: fractional cell widths must not leave dark seams in ANSI backgrounds.
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({executablePath:process.env.CHROMIUM||'/usr/bin/chromium',headless:true,args:['--no-sandbox','--disable-gpu']});
try{
 for(const dpr of [1,1.25,2]){
  const page=await browser.newPage({viewport:{width:1440,height:950},deviceScaleFactor:dpr});
  await page.route('**/api/snapshot',r=>r.fulfill({status:404,body:''}));
  await page.routeWebSocket('**/pty?*',ws=>{
   ws.send(JSON.stringify({type:'ready',pid:1}));
   ws.send(Buffer.from('\x1b[?25l\x1b[48;2;137;180;250m\x1b[2K'));
  });
  await page.goto((process.env.POC_URL||'http://127.0.0.1:4389')+'/?fallback');
  await page.waitForSelector('.pane-body[data-connected="true"]');await page.waitForTimeout(300);
  const seam=await page.locator('.pane-body canvas').first().evaluate((canvas,dpr)=>{
   const pixels=canvas.getContext('2d').getImageData(0,Math.round(8*dpr),canvas.width,1).data;
   for(let i=0;i<pixels.length;i+=4)if(pixels[i]!==137||pixels[i+1]!==180||pixels[i+2]!==250||pixels[i+3]!==255)return i/4;
   return null;
  },dpr);
  assert.equal(seam,null,`ANSI background is solid across cell edges at DPR ${dpr}`);
  await page.close();
 }
 console.log('ANSI background seams absent at 100%, 125%, and 200% scaling');
}finally{await browser.close();}
