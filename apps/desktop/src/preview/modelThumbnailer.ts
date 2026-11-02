import * as THREE from 'three';
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js';
import { RoomEnvironment } from 'three/examples/jsm/environments/RoomEnvironment.js';
import { convertFileSrc } from '@tauri-apps/api/core';
import { api } from '../api';
import { useAssetStore } from '../store';
import { modelsToRender, onModelRendered, renderKey } from './previewDone';

const SIZE = 512;
const BATCH = 20;
const IDLE_POLL_MS = 3000;
/** Three-quarter view from above, like a product shot */
const VIEW_DIRECTION = new THREE.Vector3(1, 0.8, 1).normalize();

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Renders grid thumbnails for 3D models in the background.
 *
 * The Python preview engine converts models to .glb but can't render them without a
 * display, so it leaves the .glb as the asset's thumbnail. This picks those up, renders
 * each one offscreen with a single shared WebGL renderer, and saves the PNG.
 * Returns a function that stops the loop and frees the renderer.
 */
export function startModelThumbnailer(): () => void {
  let stopped = false;

  const canvas = document.createElement('canvas');
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true });
  renderer.setSize(SIZE, SIZE, false);
  renderer.setClearColor(0x000000, 0);
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  const pmrem = new THREE.PMREMGenerator(renderer);
  const environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
  const loader = new GLTFLoader();

  async function render(modelPath: string): Promise<Uint8Array> {
    const gltf = await loader.loadAsync(convertFileSrc(modelPath));
    const model = gltf.scene;
    try {
      const sphere = new THREE.Box3().setFromObject(model).getBoundingSphere(new THREE.Sphere());
      if (!(sphere.radius > 0)) throw new Error('model has no geometry');

      const scene = new THREE.Scene();
      scene.environment = environment;
      const sun = new THREE.DirectionalLight(0xffffff, 1.5);
      sun.position.set(1, 2, 1.5);
      scene.add(sun, model);

      const camera = new THREE.PerspectiveCamera(30, 1, sphere.radius / 100, sphere.radius * 100);
      const distance = (sphere.radius / Math.sin(THREE.MathUtils.degToRad(camera.fov / 2))) * 1.05;
      camera.position.copy(sphere.center).addScaledVector(VIEW_DIRECTION, distance);
      camera.lookAt(sphere.center);

      // Export in the same task as the render, before the drawing buffer is cleared.
      renderer.render(scene, camera);
      const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
      if (!blob) throw new Error('canvas export failed');
      return new Uint8Array(await blob.arrayBuffer());
    } finally {
      disposeObject(model);
    }
  }

  async function run() {
    // Renders that failed this session. A first preview's failure isn't recorded anywhere, so it's
    // retried on next launch or when the model changes (`modelsToRender`).
    const skipped = new Set<string>();

    while (!stopped) {
      const batch = modelsToRender(
        await api.listPendingModelThumbnails(BATCH + skipped.size).catch(() => []),
        skipped
      );
      if (batch.length === 0) {
        await sleep(IDLE_POLL_MS);
        continue;
      }

      for (const item of batch) {
        const { assetId, modelPath, modifiedAt } = item;
        if (stopped) return;
        try {
          const png = await render(modelPath);
          const saved = await api.saveModelThumbnail(assetId, modifiedAt, png);
          if (saved) await onModelRendered(assetId, saved);
        } catch (err) {
          // A lost context or a stop mid-render says nothing about the model; retry later.
          if (stopped || renderer.getContext().isContextLost()) return;
          console.warn(`3D thumbnail failed for ${modelPath}:`, err);
          // A changed model becomes an error, which Regenerate retries. A report that didn't get
          // through leaves it `generating`, so it is tried again, after a pause.
          const reported = await api.failModelThumbnail(assetId, modifiedAt).catch(async () => {
            await sleep(IDLE_POLL_MS);
            return false;
          });
          if (reported) await useAssetStore.getState().refreshAssets([assetId]);
          else skipped.add(renderKey(item));
        }
      }
    }
  }

  run().finally(() => {
    environment.dispose();
    pmrem.dispose();
    renderer.dispose();
  });

  return () => {
    stopped = true;
  };
}

function disposeObject(object: THREE.Object3D) {
  object.traverse((child) => {
    const mesh = child as THREE.Mesh;
    mesh.geometry?.dispose();
    const materials = Array.isArray(mesh.material)
      ? mesh.material
      : mesh.material
        ? [mesh.material]
        : [];
    for (const material of materials) {
      for (const value of Object.values(material)) {
        if (value instanceof THREE.Texture) value.dispose();
      }
      material.dispose();
    }
  });
}
