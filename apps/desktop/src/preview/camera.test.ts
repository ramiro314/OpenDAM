import { describe, expect, test } from 'bun:test';
import * as THREE from 'three';
import {
  VIEW_DIRECTION,
  cameraUp,
  measureShape,
  seenArea,
  viewDirection,
  type Shape,
} from './camera';

const v = (x: number, y: number, z: number) => new THREE.Vector3(x, y, z);
/** The angle between two directions, in degrees */
const angle = (a: THREE.Vector3, b: THREE.Vector3) => THREE.MathUtils.radToDeg(a.angleTo(b));

/** Edge on to the usual view: at right angles to it, as SM_Waterblock_01's face is */
const edgeOn = v(1, 0, -1).normalize();

/** A shape whose box is thinnest along `thin` */
function shape(
  extents: [number, number, number],
  { thin = edgeOn, normalSum = v(0, 0, 0), area = 1, faces = new Float32Array() } = {}
): Shape {
  const long = (Math.abs(thin.y) > 0.9 ? v(1, 0, 0) : v(0, 1, 0)).cross(thin).normalize();
  const middle = thin.clone().cross(long);
  return { box: { axes: [thin, middle, long], extents }, normalSum, area, faces };
}

describe('viewDirection', () => {
  test('a non-flat box keeps the usual view', () => {
    expect(viewDirection(shape([1, 1.2, 2]))).toEqual(VIEW_DIRECTION);
    expect(viewDirection(shape([0.3, 1, 1]))).toEqual(VIEW_DIRECTION);
  });

  test('a flat box seen edge on is faced along its thinnest axis', () => {
    const direction = viewDirection(shape([0.1, 2, 3]));
    expect(angle(direction, edgeOn)).toBeLessThan(20);
  });

  test('a flat box the usual view sees well keeps it', () => {
    // A floor tile, seen from 29.5° above.
    expect(viewDirection(shape([0.1, 2, 2], { thin: v(0, 1, 0) }))).toEqual(VIEW_DIRECTION);
  });

  test('a flat box whose faces face both ways takes the side nearer the usual view', () => {
    // Its thinnest axis points down, but with no side of its own it is seen from above, as a floor.
    expect(viewDirection(shape([0.1, 2, 2], { thin: v(0, -1, 0) }))).toEqual(VIEW_DIRECTION);
  });

  test('a flat box is faced from the side its faces face', () => {
    // A ceiling: the usual view, from above, would see its back.
    const down = v(0, -1, 0);
    const direction = viewDirection(shape([0.1, 2, 2], { thin: down, normalSum: down }));
    expect(angle(direction, down)).toBeLessThan(20);
  });

  test('a bent card whose faces point away is faced from its front', () => {
    // grassPlane: a box 0.59 x 0.80 x 1.88 whose faces point to -Z, away from the usual view.
    const back = v(0, 0, -1);
    const card = shape([0.59, 0.8, 1.88], { thin: back, normalSum: v(0, 0, -0.89) });
    expect(angle(viewDirection(card), back)).toBeLessThan(20);
  });

  test('a curved card is faced where its faces point, not along its thinnest axis', () => {
    // A short trough: its thinnest axis is its length, edge on to the usual view, and its faces
    // point along its middle axis, +Y here, which the usual view sees from 29.5° above.
    const trough = (normalSum: THREE.Vector3) => shape([0.5, 1, 2], { normalSum });
    expect(viewDirection(trough(v(0, 0.64, 0)))).toEqual(VIEW_DIRECTION);
    expect(angle(viewDirection(trough(v(0, -0.64, 0))), v(0, -1, 0))).toBeLessThan(20);
  });

  test('a new view that shows less of the drawn faces keeps the usual view', () => {
    // A flat box edge on to the usual view whose drawn faces mostly face the usual view.
    const toView = VIEW_DIRECTION.clone().multiplyScalar(0.9);
    const toSide = edgeOn.clone().multiplyScalar(0.1);
    const faces = new Float32Array([...toView.toArray(), 0, ...toSide.toArray(), 0]);
    expect(viewDirection(shape([0.1, 2, 3], { faces }))).toEqual(VIEW_DIRECTION);
  });

  test('a closed shape is not a card', () => {
    const back = v(0, 0, -1);
    const closed = shape([0.59, 0.8, 1.88], { thin: back, normalSum: v(0, 0, -0.5) });
    expect(viewDirection(closed)).toEqual(VIEW_DIRECTION);
  });
});

describe('viewDirection thresholds', () => {
  const faced = (s: Shape) => !viewDirection(s).equals(VIEW_DIRECTION);

  test('a box is flat up to a thinnest side a quarter of its middle one', () => {
    expect(faced(shape([0.24, 1, 2]))).toBe(true);
    expect(faced(shape([0.25, 1, 2]))).toBe(true);
    expect(faced(shape([0.26, 1, 2]))).toBe(false);
  });

  test('a card is flat from faces whose normals sum to 0.6 of their area', () => {
    const card = (share: number) =>
      shape([0.59, 0.8, 1.88], { thin: v(0, 0, -1), normalSum: v(0, 0, -share) });
    expect(faced(card(0.61))).toBe(true);
    expect(faced(card(0.59))).toBe(false);
  });

  test('the usual view keeps a flat front it sees at 20° or more', () => {
    // A thinnest axis this many degrees off edge on, toward the usual view.
    const tilted = (degrees: number) => {
      const t = THREE.MathUtils.degToRad(degrees);
      const thin = edgeOn
        .clone()
        .multiplyScalar(Math.cos(t))
        .addScaledVector(VIEW_DIRECTION, Math.sin(t));
      return shape([0.1, 2, 3], { thin });
    };
    expect(faced(tilted(19))).toBe(true);
    expect(faced(tilted(21))).toBe(false);
  });

  test('a new view must show 1.5 times the drawn faces the usual view shows', () => {
    // One face toward the usual view and one toward the edge-on front, sized so the faced view
    // shows `gain` times what the usual view shows.
    const direction = viewDirection(shape([0.1, 2, 3]));
    const withGain = (gain: number) => {
      const side = (gain - VIEW_DIRECTION.dot(direction)) / edgeOn.dot(direction);
      const faces = new Float32Array([
        ...VIEW_DIRECTION.toArray(),
        0,
        ...edgeOn.clone().multiplyScalar(side).toArray(),
        0,
      ]);
      return shape([0.1, 2, 3], { faces });
    };
    expect(seenArea(withGain(1.45).faces, direction)).toBeCloseTo(1.45);
    expect(faced(withGain(1.55))).toBe(true);
    expect(faced(withGain(1.45))).toBe(false);
  });
});

test('seenArea counts a one-sided face only from its front', () => {
  const faces = new Float32Array([0, 0, 2, 0, 0, 0, 3, 1]);
  expect(seenArea(faces, v(0, 0, 1))).toBeCloseTo(5);
  expect(seenArea(faces, v(0, 0, -1))).toBeCloseTo(3);
});

describe('measureShape', () => {
  test('a plane seen edge on by the usual view is faced', () => {
    const plane = new THREE.Mesh(new THREE.PlaneGeometry(4, 2));
    plane.quaternion.setFromUnitVectors(v(0, 0, 1), edgeOn);
    plane.position.set(5, 1, 2);
    const group = new THREE.Group().add(plane);
    expect(angle(VIEW_DIRECTION, edgeOn)).toBeCloseTo(90);

    const measured = measureShape(group);
    expect(measured.box.extents[0]).toBeCloseTo(0);
    expect(measured.box.extents[1]).toBeCloseTo(2);
    expect(measured.box.extents[2]).toBeCloseTo(4);
    expect(measured.area).toBeCloseTo(8);

    expect(angle(viewDirection(measured), edgeOn)).toBeLessThan(20);
  });

  test('a mirrored card is read by the faces it draws', () => {
    // Mirrored on X, the plane's corners wind the other way, but three.js still draws it facing
    // +Z, which the usual view sees well.
    const plane = new THREE.Mesh(new THREE.PlaneGeometry(4, 2));
    plane.scale.set(-1, 1, 1);
    const measured = measureShape(plane);
    expect(angle(measured.normalSum, v(0, 0, 1))).toBeCloseTo(0);
    expect(viewDirection(measured)).toEqual(VIEW_DIRECTION);

    // Turned to face away from the usual view, it is faced from its drawn front.
    plane.rotation.y = Math.PI;
    expect(angle(viewDirection(measureShape(plane)), v(0, 0, -1))).toBeLessThan(20);
  });

  test('a double-sided card is seen from either side', () => {
    // Facing straight away from the usual view: one-sided, it shows nothing to it; double-sided,
    // the usual view sees all of it, and turning around would show no more.
    const away = VIEW_DIRECTION.clone().negate();
    const card = (material: THREE.Material | THREE.Material[]) => {
      const plane = new THREE.Mesh(new THREE.PlaneGeometry(4, 2), material);
      plane.quaternion.setFromUnitVectors(v(0, 0, 1), away);
      return measureShape(plane);
    };
    const front = new THREE.MeshStandardMaterial();
    const both = new THREE.MeshStandardMaterial({ side: THREE.DoubleSide });

    const oneSided = card(front);
    expect(oneSided.faces[3]).toBe(0);
    expect(angle(viewDirection(oneSided), away)).toBeLessThan(1);

    const doubleSided = card(both);
    expect(doubleSided.faces[3]).toBe(1);
    expect(viewDirection(doubleSided)).toEqual(VIEW_DIRECTION);

    // A mesh is double-sided only when all its materials are.
    expect(card([both, front]).faces[3]).toBe(0);
    expect(card([both, both]).faces[3]).toBe(1);
  });

  test('an instanced mesh is measured at every instance', () => {
    // Two 4 x 2 cards 10 apart along the mesh's own X, which its turn of 90° about Y puts along Z.
    const cards = new THREE.InstancedMesh(new THREE.PlaneGeometry(4, 2), undefined, 2);
    cards.setMatrixAt(0, new THREE.Matrix4());
    cards.setMatrixAt(1, new THREE.Matrix4().makeTranslation(10, 0, 0));
    cards.rotation.y = Math.PI / 2;

    const measured = measureShape(cards);
    expect(measured.area).toBeCloseTo(16);
    expect(measured.faces.length).toBe(2 * 2 * 4);
    expect(measured.box.extents[2]).toBeCloseTo(14);
    expect(Math.abs(measured.box.axes[2].z)).toBeCloseTo(1);
    // Both cards face the mesh's +Z, turned to world +X.
    expect(angle(measured.normalSum, v(1, 0, 0))).toBeCloseTo(0);
  });

  test('a cube keeps the usual view', () => {
    const cube = new THREE.Mesh(new THREE.BoxGeometry(1, 1, 1));
    cube.rotation.set(0.3, 0.7, 0.1);
    const measured = measureShape(cube);
    expect(measured.normalSum.length()).toBeCloseTo(0);
    expect(viewDirection(measured)).toEqual(VIEW_DIRECTION);
  });

  test('a model with no mesh keeps the usual view', () => {
    expect(viewDirection(measureShape(new THREE.Group()))).toEqual(VIEW_DIRECTION);
  });
});

test('cameraUp leaves world up unless the camera looks straight down', () => {
  expect(cameraUp(VIEW_DIRECTION)).toEqual(v(0, 1, 0));
  expect(cameraUp(v(0, 1, 0))).toEqual(v(0, 0, -1));
});
