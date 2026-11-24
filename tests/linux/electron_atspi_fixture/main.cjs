const { app, BrowserWindow, ipcMain } = require('electron');
const fs = require('node:fs');
const path = require('node:path');

app.whenReady().then(() => {
  app.setAccessibilitySupportEnabled(true);
  const win = new BrowserWindow({
    width: 500,
    height: 360,
    webPreferences: { preload: path.join(__dirname, 'receipt.cjs'), sandbox: true }
  });
  win.removeMenu();
  ipcMain.on('fixture-state', (event, state) => {
    if (event.sender !== win.webContents) return;
    const file = process.env.AAD_FIXTURE_RECEIPT;
    fs.writeFileSync(file + '.new', JSON.stringify({ electron: process.versions.electron, ...state }));
    fs.renameSync(file + '.new', file);
  });
  win.loadFile(path.join(__dirname, 'invoice.html'));
});
