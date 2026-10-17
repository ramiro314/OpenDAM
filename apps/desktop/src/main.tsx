import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import './styles/global.css';

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);

// Dev-only scroll benchmark; Vite drops this branch, and the module, from release builds.
if (import.meta.env.DEV && import.meta.env.VITE_SCROLL_BENCH) {
  const mode = import.meta.env.VITE_SCROLL_BENCH;
  void import('./dev/scrollBench').then((bench) => bench.startScrollBench(mode));
}
