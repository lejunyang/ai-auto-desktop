(() => {
  const proofs = new Map();
  const seen = new Map();
  const visibility = 'trackVisibility' in IntersectionObserver.prototype ? new IntersectionObserver(entries=>{
    for (const entry of entries) if (seen.has(entry.target)) seen.set(entry.target,entry);
  },{trackVisibility:true,delay:100}) : null;
  const watch = canvas => {
    if (!visibility || seen.has(canvas)) return;
    if (seen.size >= 32) { const first=seen.keys().next().value; visibility.unobserve(first); seen.delete(first); }
    seen.set(canvas,null); visibility.observe(canvas);
  };
  const refresh = () => {
    for (const canvas of seen.keys()) {
      visibility.unobserve(canvas);
      if (!canvas.isConnected) { seen.delete(canvas); continue; }
      seen.set(canvas,null); visibility.observe(canvas);
    }
  };
  listen('scroll',refresh);
  visualViewport?.addEventListener('resize',refresh);
  visualViewport?.addEventListener('scroll',refresh);
  const pseudoPaints = el => ['::before','::after'].some(p=>!['none','normal'].includes(getComputedStyle(el,p).content));
  const layout = canvas => {
    if (window !== top || document.visibilityState !== 'visible' || canvas.getRootNode() !== document) return;
    const box = canvas.getBoundingClientRect();
    let scaledTransform=false;
    for (let el = canvas; el; el = el.parentElement) {
      const style = getComputedStyle(el), matrix = new DOMMatrixReadOnly(style.transform === 'none' ? undefined : style.transform);
      scaledTransform ||= matrix.a !== 1 || matrix.d !== 1 || style.scale !== 'none' && style.scale !== '1';
      if (!matrix.is2D || matrix.a <= 0 || matrix.d <= 0 || matrix.b || matrix.c || style.rotate !== 'none' ||
          style.perspective !== 'none' || style.offsetPath !== 'none' || style.clipPath !== 'none' ||
          style.opacity !== '1' || style.filter !== 'none' || style.backdropFilter !== 'none' || style.maskImage !== 'none' ||
          style.mixBlendMode !== 'normal' || style.visibility !== 'visible' || style.clip !== 'auto' || pseudoPaints(el) ||
          (el === canvas || style.overflowX !== 'visible' || style.overflowY !== 'visible') &&
            ['TopLeft','TopRight','BottomLeft','BottomRight'].some(c=>parseFloat(style['border'+c+'Radius'])) ||
          (style.scale !== 'none' && !style.scale.split(/\s+/).every(v=>parseFloat(v)>0))) return;
      if (el === canvas && (style.objectFit !== 'fill' || ['Top','Right','Bottom','Left'].some(side =>
        parseFloat(style['border'+side+'Width']) || parseFloat(style['padding'+side])))) return;
    }
    return {box,scaledTransform};
  };
  for (const type of ['pointerover','pointerdown']) listen(type, event => {
    const canvas = targetOf(event);
    if (event.isTrusted && canvas?.tagName === 'CANVAS') watch(canvas);
  });
  const contains = (object, x, y) => {
    const {fx,fy,fw,fh} = object.box;
    return x >= fx && y >= fy && x < fx+fw && y < fy+fh;
  };
  return {
    prime:watch,
    stop() {
      visibility?.disconnect(); visualViewport?.removeEventListener('resize',refresh); visualViewport?.removeEventListener('scroll',refresh);
      seen.clear(); proofs.clear();
    },
    proof(doc, seq) {
      if (doc !== documentId) return null;
      const result = proofs.get(seq); proofs.delete(seq); return result ?? null;
    },
    click(canvas, event, seq) {
      try {
        if (!event.isTrusted || event.button !== 0 || event.detail < 1 || event.altKey || event.ctrlKey ||
            event.metaKey || event.shiftKey || document.elementFromPoint(event.clientX,event.clientY) !== canvas) return;
        const current = layout(canvas);
        if (!current) return {reason:'unsupported_geometry'};
        const {box,scaledTransform}=current;
        if (canvas.hasAttribute('data-aad-scene')) {
          const graph = (globalThis => (/*AAD_EDITOR*/)(null))({__aadControlV1:{scope_element:()=>canvas}});
          const hits = graph?.objects?.filter(object => contains(object,(event.clientX-box.left)/box.width,(event.clientY-box.top)/box.height));
          if (hits?.length !== 1) return {reason:'unresolved_hit'};
          const hit = hits[0], centre = hit.box;
          if (graph.objects.some(object => object !== hit && contains(object,centre.fx+centre.fw/2,centre.fy+centre.fh/2))) return;
          return {source:'editor',query:{id:hit.id}};
        }
        for (const entry of visibility?.takeRecords() ?? []) if (seen.has(entry.target)) seen.set(entry.target,entry);
        const entry = seen.get(canvas);
        if (!entry?.isIntersecting || !entry.isVisible && !scaledTransform || proofs.size >= 4 || !visualViewport ||
            ['x','y','width','height'].some(k=>Math.abs(entry.boundingClientRect[k]-box[k])>0.001)) return {reason:'unverified_visibility'};
        const rect=entry.intersectionRect, vv=visualViewport;
        const left=Math.ceil(Math.max(rect.left,vv.offsetLeft)), top=Math.ceil(Math.max(rect.top,vv.offsetTop));
        const width=Math.floor(Math.min(rect.right,vv.offsetLeft+vv.width))-left, height=Math.floor(Math.min(rect.bottom,vv.offsetTop+vv.height))-top;
        const fx=(event.clientX-left)/width,fy=(event.clientY-top)/height;
        if (!(width>0 && height>0 && fx>=0 && fx<1 && fy>=0 && fy<1)) return {reason:'unresolved_hit'};
        const density=devicePixelRatio*vv.scale;
        const sw=width*Math.min(density,canvas.width/box.width), sh=height*Math.min(density,canvas.height/box.height);
        const scale=Math.min(1,2048/sw,2048/sh,Math.sqrt(2000000/(sw*sh)));
        const rw=Math.floor(sw*scale+1e-7),rh=Math.floor(sh*scale+1e-7);
        if (!rw || !rh) return {reason:'unsupported_geometry'};
        const overlaps = other => other.width>0 && other.height>0 && other.left<left+width && other.right>left && other.top<top+height && other.bottom>top;
        const roots=[document];
        let count = 0;
        while(roots.length){
          const walker=document.createTreeWalker(roots.pop(),NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
          for (let el = walker.nextNode(); el; el = walker.nextNode()) {
            if (++count > 10000) return {reason:'unverified_visibility'};
            if (el.nodeType===Node.TEXT_NODE) {
              if (!el.length || canvas.contains(el) || ['SCRIPT','STYLE','NOSCRIPT'].includes(el.parentElement?.tagName)) continue;
              if (el.length>65536) return {reason:'unverified_visibility'};
              const range=document.createRange(); range.selectNodeContents(el);
              if (overlaps(range.getBoundingClientRect())) return {reason:'possible_overlay'};
              continue;
            }
            if (el.shadowRoot) roots.push(el.shadowRoot);
            if (el === canvas || el.contains(canvas)) continue;
            if (overlaps(el.getBoundingClientRect())) return {reason:'possible_overlay'};
            const style=getComputedStyle(el);
            if (style.display==='none') continue;
            if (el.localName.includes('-') || style.boxShadow!=='none' || style.textShadow!=='none' || style.filter!=='none' ||
                style.outlineStyle!=='none' && parseFloat(style.outlineWidth)>0 || pseudoPaints(el)) return {reason:'possible_overlay'};
          }
        }
        for (const dx of [0,Math.floor(width/2),width-1]) for (const dy of [0,Math.floor(height/2),height-1])
          if (document.elementFromPoint(left+dx,top+dy)!==canvas) return {reason:'visible_region_changed'};
        const surface=new OffscreenCanvas(rw,rh),ctx=surface.getContext('2d');
        ctx.imageSmoothingEnabled=!['pixelated','crisp-edges'].includes(getComputedStyle(canvas).imageRendering);
        ctx.drawImage(canvas,(left-box.left)/box.width*canvas.width,(top-box.top)/box.height*canvas.height,
          width/box.width*canvas.width,height/box.height*canvas.height,0,0,rw,rh);
        const {data} = ctx.getImageData(0,0,rw,rh);
        for (let alpha=3; alpha<data.length; alpha+=4) if (data[alpha] !== 255) return {reason:'translucent_pixels'};
        proofs.set(seq,surface.convertToBlob({type:'image/png'}).then(blob=>{
          if (blob.size>768*1024) return null;
          return new Promise(resolve=>{
            const reader=new FileReader(); reader.onload=()=>resolve(reader.result.split(',')[1]);
            reader.onerror=()=>resolve(null); reader.readAsDataURL(blob);
          });
        }).catch(()=>null));
        setTimeout(()=>proofs.delete(seq),10000);
        const evidence={source:'flowchart',fx,fy,rw,rh};
        if (left>Math.ceil(box.left) || top>Math.ceil(box.top) ||
            left+width<Math.floor(box.right) || top+height<Math.floor(box.bottom)) {
          evidence.view_hint={fx:(left-box.left+width/2)/box.width,fy:(top-box.top+height/2)/box.height};
        }
        return evidence;
      } catch { return {reason:'capture_failed'}; }
    }
  };
})()
