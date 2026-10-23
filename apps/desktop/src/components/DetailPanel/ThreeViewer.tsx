import { Canvas, useThree } from '@react-three/fiber';
import { useGLTF, Stage, PresentationControls, Environment } from '@react-three/drei';
import {
  Suspense,
  Component,
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
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
 * Where the forward arrow goes for a model whose bounds, in its own space, are `box` (plan
 * game-export C13): from the centre of its base along +Z, out past its front by a third of its
 * largest side. `offset` is the centring Stage gives the model, so the arrow, drawn outside
 * Stage, lines up with it without counting toward the bounds Stage centres and fits the camera to.
 */
export function arrowPlacement(box: Box3): { offset: Vector3; origin: Vector3; length: number } {
  const center = box.getCenter(new Vector3());
  const largest = Math.max(...box.getSize(new Vector3()).toArray());
  return {
    offset: center.clone().negate(),
    origin: new Vector3(center.x, box.min.y, center.z),
    length: box.max.z - center.z + largest / 3,
  };
}

/** An arrow along the model's +Z, its forward once normalized for the game, placed by `box` */
function ForwardArrow({ box }: { box: Box3 }) {
  const { offset, arrow } = useMemo(() => {
    const { offset, origin, length } = arrowPlacement(box);
    const arrow = new ArrowHelper(
      new Vector3(0, 0, 1),
      origin,
      length,
      0x3b82f6,
      length / 4,
      length / 8
    );
    return { offset, arrow };
  }, [box]);
  useEffect(() => () => arrow.dispose(), [arrow]);
  return (
    <group position={offset}>
      <primitive object={arrow} />
    </group>
  );
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
  // The model's bounds as Stage centres it, which place the forward arrow
  const [bounds, setBounds] = useState<Box3 | null>(null);
  // Stage keeps the first callback it is given, so this one never changes
  const onCentered = useCallback(
    ({ boundingBox }: { boundingBox: Box3 }) => setBounds(boundingBox.clone()),
    []
  );
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
            <Stage
              environment={null}
              intensity={0.5}
              center={forwardArrow ? { onCentered } : undefined}
              // The camera fits the model alone, so leave room for the arrow out front: a number
              // is the fit's margin
              adjustCamera={forwardArrow ? 1.6 : true}
            >
              <Model url={modelPath} />
            </Stage>
            {forwardArrow && bounds && !bounds.isEmpty() && <ForwardArrow box={bounds} />}
          </PresentationControls>
        </Suspense>
      </Canvas>
    </ThreeErrorBoundary>
  );
}
