// Evaluated in the same private world as the retained observation.
(async (args, verifyPixels) => {
  const api=globalThis.__aadControlV1;
  const canvas=api.scope_element(args);
  const unknown={error:'CONTROL.CANVAS_SCOPE_UNPROVEN'};
  if(!(canvas instanceof HTMLCanvasElement))return unknown;
  function rectangle() {
    if(api.scope_element(args)!==canvas)return {error:'CONTROL.STALE_TARGET'};
    const rect=api.rect({...args,noscroll:true});
    if(rect.error)return rect;
    const box=canvas.getBoundingClientRect(), viewport=window.visualViewport;
    if(document.visibilityState!=='visible' || devicePixelRatio!==1 || !viewport ||
       viewport.scale!==1 || viewport.offsetLeft!==0 || viewport.offsetTop!==0 ||
       box.left<0 || box.top<0 || box.right>viewport.width || box.bottom>viewport.height ||
       ![rect.x,rect.y,box.left,box.top].every(Number.isSafeInteger) ||
       canvas.width!==box.width || canvas.height!==box.height ||
       canvas.width>2048 || canvas.height>2048 || canvas.width*canvas.height>2_000_000 ||
       canvas.getRootNode()!==document) return unknown;
    for(let el=canvas;el;el=el.parentElement) {
      const s=getComputedStyle(el);
      if(s.transform!=='none' || s.translate!=='none' || s.rotate!=='none' || s.scale!=='none' ||
         s.perspective!=='none' || s.offsetPath!=='none' || s.zoom!=='1')return unknown;
      if(el===canvas && (s.objectFit!=='fill' || ['Top','Right','Bottom','Left'].some(side=>
        parseFloat(s['padding'+side])!==0 || parseFloat(s['border'+side+'Width'])!==0)))return unknown;
    }
    return rect;
  }
  function inspectSensitiveOverlaps() {
    const box=canvas.getBoundingClientRect(), roots=[document];
    let count=0;
    while(roots.length) {
      const walker=document.createTreeWalker(roots.pop(),NodeFilter.SHOW_ELEMENT);
      for(let el=walker.nextNode();el;el=walker.nextNode()) {
        if(++count>20000)return unknown;
        if(el.shadowRoot)roots.push(el.shadowRoot);
        if(!['input','iframe','frame','object','embed'].includes(el.localName))continue;
        const r=el.getBoundingClientRect();
        if(r.width>0 && r.height>0 && r.left<box.right && r.right>box.left &&
           r.top<box.bottom && r.bottom>box.top)return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
    }
    return null;
  }
  const first=rectangle();
  if(first.error)return first;
  const blocked=inspectSensitiveOverlaps();
  if(blocked)return blocked;
  if(!('trackVisibility' in IntersectionObserver.prototype) || !crypto.subtle)return unknown;
  // Use Chromium's compositor visibility signal, including covering elements
  // with pointer-events:none. A few DOM hit-test samples cannot cover a grid.
  const visible=await new Promise(resolve=>{
    const observer=new IntersectionObserver(([entry])=>finish(entry.isVisible===true &&
      entry.intersectionRatio===1 && entry.intersectionRect.width===first.width &&
      entry.intersectionRect.height===first.height),{trackVisibility:true,threshold:1,delay:100});
    const timer=setTimeout(()=>finish(false),750);
    function finish(result){clearTimeout(timer);observer.disconnect();resolve(result);}
    observer.observe(canvas);
  });
  if(!visible)return unknown;
  const current=rectangle();
  if(current.error)return current;
  const changed=inspectSensitiveOverlaps();
  if(changed)return changed;
  if(['x','y','width','height'].some(k=>current[k]!==first[k]))return unknown;
  if(!verifyPixels)return current;
  try {
    // Only a digest leaves this world. Drawing a tainted canvas keeps the
    // scratch bitmap tainted, so getImageData refuses it.
    const ctx=new OffscreenCanvas(canvas.width,canvas.height).getContext('2d');
    ctx.drawImage(canvas,0,0);
    const {data}=ctx.getImageData(0,0,canvas.width,canvas.height);
    for(let i=3;i<data.length;i+=4)if(data[i]!==255)return unknown;
    const hash=new Uint8Array(await crypto.subtle.digest('SHA-256',data));
    return {rect:current,sha256:Array.from(hash,b=>b.toString(16).padStart(2,'0')).join('')};
  } catch { return unknown; }
})
