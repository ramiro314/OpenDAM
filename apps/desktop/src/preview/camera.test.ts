import { describe, expect, test } from 'bun:test';
import * as THREE from 'three';
import { VIEW_DIRECTION, cameraUp, measureShape, viewDirection, type Shape } from './camera';

const v = (x: number, y: number, z: number) => new THREE.Vector3(x, y, z);
/** The angle between two directions, in degrees */
const angle = (a: THREE.Vector3, b: THREE.Vector3) => THREE.MathUtils.radToDeg(a.angleTo(b));

/** Edge on to the usual view: at right angles to it, as SM_Waterblock_01's face is */
const edgeOn = v(1, 0, -1).normalize();

/** A shape whose box is thinnest along `thin` */
function shape(
  extents: [number, number, number],
  { thin = edgeOn, normalSum = v(0, 0, 0), area = 1 } = {}
): Shape {
  const long = (Math.abs(thin.y) > 0.9 ? v(1, 0, 0) : v(0, 1, 0)).cross(thin).normalize();
  const middle = thin.clone().cross(long);
  return { box: { axes: [thin, middle, long], extents }, normalSum, area };
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

  test('a closed shape is not a card', () => {
    const back = v(0, 0, -1);
    const closed = shape([0.59, 0.8, 1.88], { thin: back, normalSum: v(0, 0, -0.5) });
    expect(viewDirection(closed)).toEqual(VIEW_DIRECTION);
  });
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
