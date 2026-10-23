import { Canvas, useThree } from '@react-three/fiber';
import { useGLTF, Stage, PresentationControls, Environment } from '@react-three/drei';
import { Suspense, Component, useEffect, useMemo, useState, type ReactNode } from 'react';
import { ArrowHelper, Box3, PMREMGenerator, Vector3, type Texture } from 'three';
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
 * An arrow along the model's +Z, its forward once normalized for the game (plan game-export
 * C13), from the front of its bounds, as long as half its largest side.
 */
function ForwardArrow({ url }: { url: string }) {
  const { scene } = useGLTF(convertFileSrc(url), false, false);
  const arrow = useMemo(() => {
    // In the scene's own space, whatever Stage has moved it by
    scene.updateMatrixWorld(true);
    const box = new Box3().setFromObject(scene).applyMatrix4(scene.matrixWorld.clone().invert());
    const size = box.isEmpty() ? 1 : Math.max(...box.getSize(new Vector3()).toArray()) || 1;
    const length = size / 2;
    const front = new Vector3(0, 0, box.isEmpty() ? 0 : box.max.z);
    return new ArrowHelper(new Vector3(0, 0, 1), front, length, 0x3b82f6, length / 3, length / 6);
  }, [scene]);
  useEffect(() => () => arrow.dispose(), [arrow]);
  return <primitive object={arrow} />;
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

/**
 * A 3D view of the GLB at `modelPath`. With `forwardArrow`, an arrow marks its +Z and the camera
 * starts at a three-quarter view from the front, so the way it faces shows.
 */
export function ThreeViewer({
  modelPath,
  forwardArrow = false,
}: {
  modelPath: string;
  forwardArrow?: boolean;
}) {
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
      <Canvas
        shadows
        dpr={[1, 2]}
        camera={forwardArrow ? { fov: 50, position: [3, 2, 4] } : { fov: 50 }}
      >
        <RoomLighting />
        <Suspense fallback={null}>
          <PresentationControls speed={1.5} global zoom={0.5} polar={[-0.1, Math.PI / 4]}>
            <Stage environment={null} intensity={0.5}>
              <Model url={modelPath} />
              {forwardArrow && <ForwardArrow url={modelPath} />}
            </Stage>
          </PresentationControls>
        </Suspense>
      </Canvas>
    </ThreeErrorBoundary>
  );
}
