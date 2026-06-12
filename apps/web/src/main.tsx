import { useEffect, useState } from 'react';
import ReactDOM from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import App from './App';
import AppSplash from './AppSplash';
import './selran-theme.css'; // shared Selran suite theme — load FIRST
import './index.css';
import '@xyflow/react/dist/style.css';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      refetchOnWindowFocus: false,
    },
  },
});

function Root() {
  // Paint the splash overlay first, then mount the (heavy) app one tick later so
  // its boot-time re-renders happen BEHIND the already-visible splash — no
  // flash of the app UI before the splash. (StrictMode's dev double-mount also
  // added to the flicker, so the splash is mounted plainly.)
  const [showApp, setShowApp] = useState(false);
  useEffect(() => {
    const id = window.setTimeout(() => setShowApp(true), 180);
    return () => window.clearTimeout(id);
  }, []);
  return (
    <>
      {showApp ? (
        <QueryClientProvider client={queryClient}>
          <App />
        </QueryClientProvider>
      ) : null}
      <AppSplash />
    </>
  );
}

ReactDOM.createRoot(document.getElementById('root')!).render(<Root />);
