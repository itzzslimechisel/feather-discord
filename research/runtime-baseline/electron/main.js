const { app, BrowserWindow } = require('electron');
const path = require('path');
app.whenReady().then(() => {
  const w = new BrowserWindow({ width: 1000, height: 700, webPreferences: { sandbox: true, contextIsolation: true } });
  w.loadFile(path.join(__dirname, '..', 'page', 'index.html'));
  w.webContents.on('page-title-updated', (_e, t) => { if (t === 'painted') console.log('PAINTED ' + Date.now()); });
});
