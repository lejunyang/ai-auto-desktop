const { ipcRenderer } = require('electron');

window.addEventListener('DOMContentLoaded', () => {
  const approved = document.getElementById('approved');
  const other = document.getElementById('other');
  const changes = [];
  const publish = () => ipcRenderer.send('fixture-state', {
    hash: location.hash, approved: approved.checked, other: other.checked, changes
  });
  document.addEventListener('change', event => {
    changes.push([event.target.id, event.target.checked]);
    publish();
  });
  window.addEventListener('hashchange', publish);
  publish();
});
