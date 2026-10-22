import { Canvas, useThree } from '@react-three/fiber';
import { useGLTF, Stage, PresentationControls, Environment } from '@react-three/drei';
import { Suspense, Component, useEffect, useState, type ReactNode } from 'react';
import { PMREMGenerator, type Texture } from 'three';
import { RoomEnvironment } from 'three/examples/jsm/environments/RoomEnvironment.js';
import { convertFileSrc } from '@tauri-apps/api/core';
import { useAssetStore } from '../../store';

interface ErrorBoundaryProps {
  fallback: ReactNode;
  children: ReactNode;
}

interface ErrorBoundaryState {
  hasError: boolean;
}

class ThreeErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  constructor(props: ErrorBoundaryProps) {
    super(props);
    this.state = { hasError: false };
  }

  static getDerivedStateFromError() {
    return { hasError: true };
  }

  componentDidCatch(error: any, errorInfo: any) {
    console.error('ThreeViewer render error:', error, errorInfo);
  }

  render() {
    if (this.state.hasError) {
      return this.props.fallback;
    }
    return this.props.children;
  }
}

export function Model({ url }: { url: string }) {
  const fileUrl = convertFileSrc(url);
  // No Draco or meshopt: their decoders come from a CDN or inline WASM, which the CSP blocks.
  const { scene } = useGLTF(fileUrl, false, false);
  return <primitive object={scene} />;
}

/**
 * Shows a rerun preview model: a rerun overwrites the same `<id>_preview.glb`, so this drops
 * drei's cached copy, under the converted URL `Model` loads it by, and bumps the path's
 * revision, which remounts every open viewer of it so it loads the new file.
 */
export function reloadModel(path: string) {
  useGLTF.clear(convertFileSrc(path));
  useAssetStore.getState().bumpModelRevision(path);
}

/** Lights the scene with a generated room, as the thumbnailer does, instead of a downloaded HDR. */
function RoomLighting() {
  const gl = useThree((state) => state.gl);
  const [map, setMap] = useState<Texture | null>(null);
  // Created in the effect, not memoized, so each mount (StrictMode mounts twice) owns and frees
  // its own render target.
  useEffect(() => {
    const pmrem = new PMREMGenerator(gl);
    const room = new RoomEnvironment();
    const target = pmrem.fromScene(room, 0.04);
    room.dispose();
    pmrem.dispose();
    setMap(target.texture);
    return () => {
      setMap(null);
      target.dispose();
    };
  }, [gl]);
  return map ? <Environment map={map} /> : null;
}

export function ThreeViewer({ modelPath }: { modelPath: string }) {
  // Each rerun remounts the whole viewer, canvas included, as reopening it would. Remounting only
  // `Model` inside the live canvas loads the new GLB but leaves it out of the scene.
  const revision = useAssetStore((s) => s.modelRevisions[modelPath] ?? 0);
  return (
    <ThreeErrorBoundary
      key={revision}
      fallback={
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            height: '100%',
            color: 'var(--color-text-muted)',
            fontSize: 13,
          }}
        >
          Unable to render 3D preview
        </div>
      }
    >
      <Canvas shadows dpr={[1, 2]} camera={{ fov: 50 }}>
        <RoomLighting />
        <Suspense fallback={null}>
          <PresentationControls speed={1.5} global zoom={0.5} polar={[-0.1, Math.PI / 4]}>
            <Stage environment={null} intensity={0.5}>
              <Model url={modelPath} />
            </Stage>
          </PresentationControls>
        </Suspense>
      </Canvas>
    </ThreeErrorBoundary>
  );
}
