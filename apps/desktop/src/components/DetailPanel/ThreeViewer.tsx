import { Canvas, useThree } from '@react-three/fiber';
import { useGLTF, Stage, PresentationControls, Environment } from '@react-three/drei';
import { Suspense, Component, useEffect, useMemo, type ReactNode } from 'react';
import { PMREMGenerator } from 'three';
import { RoomEnvironment } from 'three/examples/jsm/environments/RoomEnvironment.js';
import { convertFileSrc } from '@tauri-apps/api/core';

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

function Model({ url }: { url: string }) {
  const fileUrl = convertFileSrc(url);
  // No Draco or meshopt: their decoders come from a CDN or inline WASM, which the CSP blocks.
  const { scene } = useGLTF(fileUrl, false, false);
  return <primitive object={scene} />;
}

/** Lights the scene with a generated room, as the thumbnailer does, instead of a downloaded HDR. */
function RoomLighting() {
  const gl = useThree((state) => state.gl);
  const map = useMemo(() => {
    const pmrem = new PMREMGenerator(gl);
    const texture = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
    pmrem.dispose();
    return texture;
  }, [gl]);
  useEffect(() => () => map.dispose(), [map]);
  return <Environment map={map} />;
}

export function ThreeViewer({ modelPath }: { modelPath: string }) {
  return (
    <ThreeErrorBoundary
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
