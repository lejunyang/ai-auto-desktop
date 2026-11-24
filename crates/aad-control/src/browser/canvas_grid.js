// The retained canvas, its visible rectangular intersection and the reference
// rendering used to verify a compositor screenshot. All boxes use CSS pixels
// until the final raster is allocated, including at non-unit device scale.
(async (args, mode) => {
  const api = globalThis.__aadControlV1;
  const canvas = api.scope_element(args);
  const unknown = {error: 'CONTROL.CANVAS_SCOPE_UNPROVEN'};
  if (!(canvas instanceof HTMLCanvasElement)) return unknown;
  const original = api.rect({...args, noscroll:true});
  if (original.error) return original;
  if (!window.visualViewport || !crypto.subtle || !('trackVisibility' in IntersectionObserver.prototype) ||
      canvas.getRootNode() !== document || document.visibilityState !== 'visible') return unknown;
  const entry = await new Promise(resolve => {
    const watcher = new IntersectionObserver(([entry]) => finish(entry), {trackVisibility:true, delay:100});
    const timer = setTimeout(() => finish(null), 750);
    function finish(value) { clearTimeout(timer); watcher.disconnect(); resolve(value); }
    watcher.observe(canvas);
  });
  if (!entry?.isIntersecting || api.scope_element(args) !== canvas) return unknown;
  const rect = api.rect({...args, noscroll:true});
  if (rect.error) return rect;
  const box = canvas.getBoundingClientRect(), intersection = entry.intersectionRect, vv = window.visualViewport;
  const sameRect = (a,b) => ['x','y','width','height'].every(k=>Math.abs(a[k]-b[k])<0.001);
  if (!sameRect(rect, original) || !sameRect(entry.boundingClientRect, box)) return unknown;
  const pseudoPaints = el => ['::before','::after'].some(p =>
    !['none','normal'].includes(getComputedStyle(el,p).content));
  for (let el=canvas; el; el=el.parentElement) {
    const s=getComputedStyle(el), m=new DOMMatrixReadOnly(s.transform==='none'?undefined:s.transform);
    if (!m.is2D || m.a<=0 || m.d<=0 || m.b!==0 || m.c!==0 || s.perspective!=='none' ||
        s.rotate!=='none' || s.offsetPath!=='none' || s.clipPath!=='none' ||
        s.opacity!=='1' || s.filter!=='none' || s.backdropFilter!=='none' || s.maskImage!=='none' ||
        s.mixBlendMode!=='normal' || s.visibility!=='visible' || s.clip!=='auto' || pseudoPaints(el) ||
        (el===canvas || s.overflowX!=='visible' || s.overflowY!=='visible') &&
        ['TopLeft','TopRight','BottomLeft','BottomRight'].some(c=>parseFloat(s['border'+c+'Radius'])!==0) ||
        (s.scale!=='none' && !s.scale.split(/\s+/).every(v=>parseFloat(v)>0))) return unknown;
    if (el===canvas && (s.objectFit!=='fill' || ['Top','Right','Bottom','Left'].some(side=>
      parseFloat(s['padding'+side])!==0 || parseFloat(s['border'+side+'Width'])!==0))) return unknown;
  }
  // Round inwards. A pixel that straddles the clip or a scrollbar does not
  // belong to this view. Integer CSS clips also work with fractional DPR.
  const x=Math.ceil(Math.max(intersection.left, vv.offsetLeft) + scrollX);
  const y=Math.ceil(Math.max(intersection.top, vv.offsetTop) + scrollY);
  const right=Math.floor(Math.min(intersection.right, vv.offsetLeft+vv.width) + scrollX);
  const bottom=Math.floor(Math.min(intersection.bottom, vv.offsetTop+vv.height) + scrollY);
  const width=right-x, height=bottom-y;
  if (width<1 || height<1 || !canvas.width || !canvas.height) return unknown;
  const region={fx:(x-rect.x)/rect.width,fy:(y-rect.y)/rect.height,fw:width/rect.width,fh:height/rect.height};
  const dpr=devicePixelRatio, zoom=vv.scale;
  const sampleWidth=Math.min(width*dpr*zoom,region.fw*canvas.width);
  const sampleHeight=Math.min(height*dpr*zoom,region.fh*canvas.height);
  const factor=Math.min(1,2048/sampleWidth,2048/sampleHeight,Math.sqrt(2000000/(sampleWidth*sampleHeight)));
  const rw=Math.max(1,Math.floor(sampleWidth*factor+1e-7)), rh=Math.max(1,Math.floor(sampleHeight*factor+1e-7));
  const sourceView=width!==canvas.width || height!==canvas.height || rw!==width || rh!==height ||
    rect.width!==width || rect.height!==height || dpr!==1 || zoom!==1;
  const roots=[document]; let count=0;
  while (roots.length) {
    const walker=document.createTreeWalker(roots.pop(),NodeFilter.SHOW_ELEMENT);
    for(let el=walker.nextNode();el;el=walker.nextNode()) {
      if(++count>20000)return unknown;
      if(el.shadowRoot)roots.push(el.shadowRoot);
      if(el===canvas || el.contains(canvas))continue;
      const sensitive=['input','iframe','frame','object','embed'].includes(el.localName);
      if(!sensitive && entry.isVisible && !sourceView)continue;
      const r=el.getBoundingClientRect();
      if(r.width>0 && r.height>0 && r.left+scrollX<right && r.right+scrollX>x &&
          r.top+scrollY<bottom && r.bottom+scrollY>y) {
        return sensitive ? {error:'CONTROL.ACTION_UNSUPPORTED'} : unknown;
      }
      if(!entry.isVisible && r.width>0 && r.height>0) {
        const style=getComputedStyle(el);
        if(style.boxShadow!=='none' || style.textShadow!=='none' || style.filter!=='none' ||
            parseFloat(style.outlineWidth)>0 || pseudoPaints(el))return unknown;
      }
    }
  }
  // Chromium can report isVisible=false for an otherwise unobstructed
  // downscaled transform. Require hits inside the clipped rectangle as well.
  if(!entry.isVisible || sourceView) {
    for(const dx of [0,Math.floor(width/2),width-1])for(const dy of [0,Math.floor(height/2),height-1]) {
      if(document.elementFromPoint(x-scrollX+dx,y-scrollY+dy)!==canvas)return unknown;
    }
  }
  const layout={x,y,width,height,rw,rh,dpr,zoom,sourceView,region,
    intrinsicWidth:canvas.width,intrinsicHeight:canvas.height,element:rect};
  if(mode==='geometry')return layout;
  try {
    const surface=document.createElement('canvas'); surface.width=rw; surface.height=rh;
    const ctx=surface.getContext('2d');
    ctx.imageSmoothingEnabled=!['pixelated','crisp-edges'].includes(getComputedStyle(canvas).imageRendering);
    ctx.drawImage(canvas,region.fx*canvas.width,region.fy*canvas.height,region.fw*canvas.width,
      region.fh*canvas.height,0,0,rw,rh);
    const {data}=ctx.getImageData(0,0,rw,rh);
    for(let i=3;i<data.length;i+=4)if(data[i]!==255)return unknown;
    const png=mode==='reference'?surface.toDataURL('image/png').split(',')[1]:undefined;
    const hash=new Uint8Array(await crypto.subtle.digest('SHA-256',data));
    return {rect:layout,sha256:Array.from(hash,b=>b.toString(16).padStart(2,'0')).join(''),data:png};
  } catch { return unknown; }
})
