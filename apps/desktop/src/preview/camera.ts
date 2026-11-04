import * as THREE from 'three';

/** Three-quarter view from above, like a product shot */
export const VIEW_DIRECTION = new THREE.Vector3(1, 0.8, 1).normalize();
/** A box whose thinnest side is at most this share of its middle one is flat */
const FLAT_RATIO = 0.25;
/** Faces whose normals sum to at least this share of their area point one way, as a card's do */
const SHEET_RATIO = 0.6;
/** The usual view keeps a flat model whose front it sees at least this steeply, as a floor's */
const MIN_FRONT_ANGLE = THREE.MathUtils.degToRad(20);
/** A flat model's face outweighs the usual view this much, so the camera stays within 19.5° of it */
const FACE_WEIGHT = 3;
/** A new view must show this many times the drawn face area the usual view shows */
const MIN_GAIN = 1.5;

/** A model's oriented bounding box: its principal axes, thinnest first, and its extent along each */
export interface OrientedBox {
  axes: [THREE.Vector3, THREE.Vector3, THREE.Vector3];
  extents: [number, number, number];
}

/** What the camera reads of a model, in world space */
export interface Shape {
  box: OrientedBox;
  /** The sum of each triangle's normal times its area */
  normalSum: THREE.Vector3;
  area: number;
  /**
   * Each triangle as it is drawn: its normal times its area, then 1 when both its sides are
   * drawn, else 0, four numbers a triangle
   */
  faces: Float32Array;
}

/**
 * The direction from a model's center to the camera.
 *
 * A model is flat when its oriented box is thin, or when its faces mostly point one way, as a
 * bent card's do. Its front is the side of its box's thinnest axis its faces face, or, when they
 * face both ways, the side nearer the usual view, and a card's front is where its faces point.
 * When the usual view sees that front edge on or from behind, the model is faced from its front
 * instead, leaning a little toward the usual view, as long as that shows clearly more of its
 * drawn faces. Anything else gets the usual three-quarter view.
 */
export function viewDirection({ box, normalSum, area, faces }: Shape): THREE.Vector3 {
  const [thin, middle] = box.extents;
  const thinBox = middle > 0 && thin <= FLAT_RATIO * middle;
  const sheet = area > 0 && normalSum.length() >= SHEET_RATIO * area;
  if (!thinBox && !sheet) return VIEW_DIRECTION.clone();

  // A card faces where its faces point, which a curved one's thinnest axis may not.
  const axis = thinBox ? box.axes[0].clone() : normalSum.clone().normalize();
  const facing = normalSum.dot(axis);
  const side =
    Math.abs(facing) >= 0.25 * area ? Math.sign(facing) : Math.sign(axis.dot(VIEW_DIRECTION)) || 1;
  const front = axis.multiplyScalar(side);
  if (front.dot(VIEW_DIRECTION) >= Math.sin(MIN_FRONT_ANGLE)) return VIEW_DIRECTION.clone();
  const faced = front.multiplyScalar(FACE_WEIGHT).add(VIEW_DIRECTION).normalize();
  const gain = seenArea(faces, faced) >= MIN_GAIN * seenArea(faces, VIEW_DIRECTION);
  return gain ? faced : VIEW_DIRECTION.clone();
}

/** The area of the drawn faces a camera in `direction` sees, as if none hid another */
export function seenArea(faces: Float32Array, direction: THREE.Vector3): number {
  let seen = 0;
  for (let i = 0; i + 3 < faces.length; i += 4) {
    const dot = faces[i] * direction.x + faces[i + 1] * direction.y + faces[i + 2] * direction.z;
    seen += faces[i + 3] ? Math.abs(dot) : Math.max(0, dot);
  }
  return seen;
}

/**
 * The direction from a model's center to the camera, for the thumbnailer. A model whose shape
 * can't be read, such as one too large to hold its faces, gets the usual view, as every model did
 * before, rather than failing its thumbnail.
 */
export function cameraDirection(model: THREE.Object3D): THREE.Vector3 {
  try {
    return viewDirection(measureShape(model));
  } catch (err) {
    console.warn('3D thumbnail camera fell back to the usual view:', err);
    return VIEW_DIRECTION.clone();
  }
}

/** The camera's up: world up, unless the camera looks almost straight up or down */
export function cameraUp(direction: THREE.Vector3): THREE.Vector3 {
  return Math.abs(direction.y) > 0.99 ? new THREE.Vector3(0, 0, -1) : new THREE.Vector3(0, 1, 0);
}

/** Reads the shape of every mesh under `object`, and every instance of one, in world space */
export function measureShape(object: THREE.Object3D): Shape {
  object.updateWorldMatrix(true, true);
  // Each mesh at its world transform, or an instanced one at each instance's, as it is drawn.
  const parts: { mesh: THREE.Mesh; matrix: THREE.Matrix4 }[] = [];
  object.traverse((child) => {
    const mesh = child as THREE.Mesh;
    if (!mesh.isMesh || !mesh.geometry.attributes.position) return;
    const instanced = mesh as THREE.InstancedMesh;
    if (!instanced.isInstancedMesh) {
      parts.push({ mesh, matrix: mesh.matrixWorld });
      return;
    }
    for (let i = 0; i < instanced.count; i++) {
      const matrix = new THREE.Matrix4();
      instanced.getMatrixAt(i, matrix);
      parts.push({ mesh, matrix: matrix.premultiply(mesh.matrixWorld) });
    }
  });

  const point = new THREE.Vector3();
  const eachPoint = (visit: (p: THREE.Vector3) => void) => {
    for (const { mesh, matrix } of parts) {
      const position = mesh.geometry.attributes.position;
      for (let i = 0; i < position.count; i++) {
        visit(point.fromBufferAttribute(position, i).applyMatrix4(matrix));
      }
    }
  };

  const normalSum = new THREE.Vector3();
  let area = 0;
  const triangles = parts.reduce(
    (n, { mesh }) =>
      n + Math.floor((mesh.geometry.index ?? mesh.geometry.attributes.position).count / 3),
    0
  );
  const faces = new Float32Array(triangles * 4);
  let face = 0;
  const a = new THREE.Vector3();
  const b = new THREE.Vector3();
  const c = new THREE.Vector3();
  for (const { mesh, matrix } of parts) {
    const { index, attributes } = mesh.geometry;
    const position = attributes.position;
    const count = index ? index.count : position.count;
    // A mirrored mesh draws its faces with the opposite winding (three.js flips its front face).
    const winding = matrix.determinant() < 0 ? -0.5 : 0.5;
    const materials = Array.isArray(mesh.material) ? mesh.material : [mesh.material];
    const doubleSided = materials.every((material) => material.side === THREE.DoubleSide);
    const corner = (v: THREE.Vector3, i: number) =>
      v.fromBufferAttribute(position, index ? index.getX(i) : i).applyMatrix4(matrix);
    for (let i = 0; i + 2 < count; i += 3) {
      corner(a, i);
      corner(b, i + 1).sub(a);
      corner(c, i + 2).sub(a);
      const cross = b.cross(c);
      area += cross.length() / 2;
      cross.multiplyScalar(winding);
      normalSum.add(cross);
      faces.set([cross.x, cross.y, cross.z, doubleSided ? 1 : 0], face);
      face += 4;
    }
  }

  return { box: orientedBox(eachPoint), normalSum, area, faces };
}

/** The oriented bounding box of a point set, from its principal axes */
export function orientedBox(eachPoint: (visit: (p: THREE.Vector3) => void) => void): OrientedBox {
  const mean = new THREE.Vector3();
  let n = 0;
  eachPoint((p) => {
    mean.add(p);
    n++;
  });
  if (n === 0) return { axes: worldAxes(), extents: [0, 0, 0] };
  mean.divideScalar(n);

  const cov = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
  ];
  eachPoint((p) => {
    const d = [p.x - mean.x, p.y - mean.y, p.z - mean.z];
    for (let i = 0; i < 3; i++) for (let j = 0; j < 3; j++) cov[i][j] += d[i] * d[j];
  });

  const axes = eigenvectors(cov);
  const lo = [Infinity, Infinity, Infinity];
  const hi = [-Infinity, -Infinity, -Infinity];
  eachPoint((p) => {
    axes.forEach((axis, i) => {
      const t = p.dot(axis);
      lo[i] = Math.min(lo[i], t);
      hi[i] = Math.max(hi[i], t);
    });
  });
  const order = [0, 1, 2].sort((i, j) => hi[i] - lo[i] - (hi[j] - lo[j]));
  return {
    axes: order.map((i) => axes[i]) as OrientedBox['axes'],
    extents: order.map((i) => hi[i] - lo[i]) as OrientedBox['extents'],
  };
}

function worldAxes(): OrientedBox['axes'] {
  return [new THREE.Vector3(1, 0, 0), new THREE.Vector3(0, 1, 0), new THREE.Vector3(0, 0, 1)];
}

/** The eigenvectors of a symmetric 3x3 matrix, by Jacobi rotations (changes `m`) */
function eigenvectors(m: number[][]): THREE.Vector3[] {
  const v = [
    [1, 0, 0],
    [0, 1, 0],
    [0, 0, 1],
  ];
  const scale = Math.abs(m[0][0]) + Math.abs(m[1][1]) + Math.abs(m[2][2]);
  for (let sweep = 0; sweep < 50; sweep++) {
    let p = 0;
    let q = 1;
    for (const [i, j] of [
      [0, 2],
      [1, 2],
    ]) {
      if (Math.abs(m[i][j]) > Math.abs(m[p][q])) [p, q] = [i, j];
    }
    if (Math.abs(m[p][q]) <= 1e-12 * scale) break;
    const theta = 0.5 * Math.atan2(2 * m[p][q], m[q][q] - m[p][p]);
    const cos = Math.cos(theta);
    const sin = Math.sin(theta);
    for (let k = 0; k < 3; k++) {
      [m[k][p], m[k][q]] = [cos * m[k][p] - sin * m[k][q], sin * m[k][p] + cos * m[k][q]];
    }
    for (let k = 0; k < 3; k++) {
      [m[p][k], m[q][k]] = [cos * m[p][k] - sin * m[q][k], sin * m[p][k] + cos * m[q][k]];
    }
    for (let k = 0; k < 3; k++) {
      [v[k][p], v[k][q]] = [cos * v[k][p] - sin * v[k][q], sin * v[k][p] + cos * v[k][q]];
    }
  }
  return [0, 1, 2].map((i) => new THREE.Vector3(v[0][i], v[1][i], v[2][i]));
}
