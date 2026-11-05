// How the browser names an element: its role and its accessible name.
// Shared by the control session (aad-control's browser.js), which finds
// elements by these, and the recorder (observer.js), which writes them down
// so a recorded step finds the same element when it is replayed.
  const inputRole = el => ({checkbox:'checkbox',radio:'radio',button:'button',submit:'button',reset:'button',range:'slider',file:'button'}[(el.type||'').toLowerCase()] || 'textbox');
  const roleOf = el => el.getAttribute('role') || (el.tagName==='INPUT' ? inputRole(el) : ({TEXTAREA:'textbox',BUTTON:'button',A:'link',SELECT:'combobox',BODY:'document',IMG:'img',CANVAS:'canvas',SVG:'img',LI:'listitem',UL:'list',OL:'list',TABLE:'table',TR:'row',TD:'cell',TH:'columnheader',H1:'heading',H2:'heading',H3:'heading',H4:'heading',H5:'heading',H6:'heading',DIALOG:'dialog',FORM:'form',HEADER:'banner',NAV:'navigation',MAIN:'main',ASIDE:'complementary',FOOTER:'contentinfo',SECTION:'region',OUTPUT:'status'}[el.tagName.toUpperCase()] || 'generic'));
  const textOf = el => {let text='',i=0;const walker=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node;while(text.length<256 && i++<32 && (node=walker.nextNode()))text+=(node.nodeValue||'').slice(0,256-text.length);return text;};
  const labelOf = el => (el.getAttribute('aria-label') || (el.labels && Array.from(el.labels).slice(0,8).map(textOf).join(' ')) || el.getAttribute('alt') || el.getAttribute('title') || el.getAttribute('placeholder') || (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : textOf(el)) || '').trim().slice(0,256);
