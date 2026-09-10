// One instanced draw for pane surfaces, borders, and the drop preview.
// DOM handles text and controls. Rendering is requested only on changes.
export async function createRenderer(canvas, status, invalidate) {
  let device, buffer, context;
  try {
    if (new URLSearchParams(location.search).has('fallback')) throw Error('requested');
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) throw Error('No WebGPU adapter');
    device = await adapter.requestDevice();
    device.pushErrorScope('validation');
    const module = device.createShaderModule({code:`
      struct Rect { bounds: vec4f, color: vec4f }
      @group(0) @binding(0) var<storage, read> rects: array<Rect>;
      struct Out { @builtin(position) position: vec4f, @location(0) color: vec4f }
      @vertex fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) i: u32) -> Out {
        let points = array<vec2f,6>(vec2f(0,0),vec2f(1,0),vec2f(0,1),vec2f(0,1),vec2f(1,0),vec2f(1,1));
        let r = rects[i]; let p = r.bounds.xy + points[v] * r.bounds.zw;
        var out: Out; out.position = vec4f(p.x*2-1,1-p.y*2,0,1); out.color = r.color; return out;
      }
      @fragment fn fs(in: Out) -> @location(0) vec4f { return in.color; }
    `});
    const format = navigator.gpu.getPreferredCanvasFormat();
    const pipeline = await device.createRenderPipelineAsync({layout:'auto',vertex:{module,entryPoint:'vs'},fragment:{module,entryPoint:'fs',targets:[{format}]},primitive:{topology:'triangle-list'}});
    buffer = device.createBuffer({size:32768,usage:GPUBufferUsage.STORAGE|GPUBufferUsage.COPY_DST});
    const bind = device.createBindGroup({layout:pipeline.getBindGroupLayout(0),entries:[{binding:0,resource:{buffer}}]});
    const error = await device.popErrorScope(); if (error) throw Error(error.message);
    context = canvas.getContext('webgpu');
    context.configure({device,format,alphaMode:'opaque'});
    status.textContent='● WebGPU · pane compositor';
    let lost=false;
    device.lost.then(info=>{lost=true;status.title=info.message;status.textContent='Canvas fallback · GPU disconnected';invalidate();});
    let fallback;
    const data = new Float32Array(8192);
    return (rects,w,h) => {
      if(lost){fallback ??= makeFallback(canvas,status);fallback(rects,w,h);return;}
      const dpr = Math.min(devicePixelRatio || 1,2);
      const pw=Math.max(1,Math.min(device.limits.maxTextureDimension2D,Math.round(w*dpr))), ph=Math.max(1,Math.min(device.limits.maxTextureDimension2D,Math.round(h*dpr)));
      if(canvas.width!==pw||canvas.height!==ph){canvas.width=pw;canvas.height=ph;}
      let offset=0;
      for(const r of rects){data.set([r.x/w,r.y/h,r.w/w,r.h/h,...r.color],offset);offset+=8;}
      if(offset) device.queue.writeBuffer(buffer,0,data,0,offset);
      const encoder=device.createCommandEncoder();
      const pass=encoder.beginRenderPass({colorAttachments:[{view:context.getCurrentTexture().createView(),clearValue:{r:.063,g:.071,b:.094,a:1},loadOp:'clear',storeOp:'store'}]});
      pass.setPipeline(pipeline);pass.setBindGroup(0,bind);pass.draw(6,rects.length);pass.end();
      device.queue.submit([encoder.finish()]);
    };
  } catch(error) {
    buffer?.destroy();device?.destroy();
    status.title=error.message;
    return makeFallback(canvas,status);
  }
}
function makeFallback(canvas,status) {
  // A canvas cannot switch context types after WebGPU initialization.
  const replacement=canvas.cloneNode();canvas.replaceWith(replacement);
  const ctx=replacement.getContext('2d');
  status.textContent='● Canvas fallback · WebGPU unavailable';
  return (rects,w,h)=>{
    const dpr=Math.min(devicePixelRatio||1,2);
    if(replacement.width!==Math.round(w*dpr)||replacement.height!==Math.round(h*dpr)){replacement.width=Math.round(w*dpr);replacement.height=Math.round(h*dpr);}
    ctx.setTransform(dpr,0,0,dpr,0,0);ctx.clearRect(0,0,w,h);
    for(const r of rects){ctx.fillStyle=`rgba(${r.color.slice(0,3).map(c=>Math.round(c*255)).join(',')},${r.color[3]})`;ctx.fillRect(r.x,r.y,r.w,r.h);}
  };
}
