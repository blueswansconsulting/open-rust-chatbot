"""Builds src/data/outcode-centroids.csv from OS Code-Point Open (CSV edition, GB).

    python3 src/data/build-outcode-centroids.py codepo_gb.zip src/data/outcode-centroids.csv

Download: https://api.os.uk/downloads/v1/products/CodePointOpen/downloads?area=GB&format=CSV&redirect
Licence: Open Government Licence v3 (see src/data/NOTICE for the attribution it requires).

Each output line is "OUTCODE,lat,lon" (WGS84, 4 decimal places, about 10 m), sorted by outcode:
the mean position of the outcode's postcodes. Postcodes without real coordinates (positional
quality 60, a sector mean used for PO boxes, and 90, none) are skipped, so outcodes made only of
them (non-geographic ones) are left out. Code-Point Open covers Great Britain only: no Northern
Ireland (BT), Channel Islands or Isle of Man.
"""
import csv
import io
import math
import sys
import zipfile

# Non-geographic districts (PO boxes, large users, reply services) that still have a few located
# postcodes in Code-Point Open: the districts Royal Mail marks non-geographic that have fewer than
# 100 located postcodes. Districts that are only partly non-geographic (BA9, CR9, EC1P, …) stay.
NON_GEOGRAPHIC = set("""
AB99 B99 BD97 BD98 BD99 BL11 BL78 BN50 BN51 BN52 BN88 BS98 BS99 CA95 CF30 CF91 CF95
CF99 CH25 CH26 CH27 CH28 CH29 CH30 CH31 CH32 CH33 CH34 CH70 CH88 CH99 CM92 CM98 CM99
CR90 CT50 CW98 DE99 DH97 DH98 DH99 DL98 E98 EC2P EC3P EC4P EH91 EH95 EH99 G58 G70 G79
G90 IP98 IV99 KY99 L67 L68 L70 L71 L72 L74 L75 LE21 LE41 LE87 LE94 LE95 LS88 LS98 LS99
M61 M99 MK77 N81 NE85 NE88 NE92 NE98 NE99 NG80 NG90 NR99 NW26 PL95 PR0 PR11 RH77 S49
S95 S96 S97 S99 SA80 SA99 SL60 SL95 SN99 SO25 SO97 SS22 SS99 ST55 SW95 SY99 UB18 WA55
WA88 WD99 WF90 WR99 YO90
""".split())


def osgb36_to_wgs84(e, n):
    """National Grid easting/northing (OSGB36) to WGS84 lat/lon in degrees (Helmert, ~5 m)."""
    # Airy 1830 ellipsoid and the National Grid projection.
    a, b = 6377563.396, 6356256.909
    f0, lat0, lon0 = 0.9996012717, math.radians(49), math.radians(-2)
    n0, e0 = -100000.0, 400000.0
    e2 = 1 - (b * b) / (a * a)
    nn = (a - b) / (a + b)
    lat, m = lat0, 0.0
    while True:
        lat = (n - n0 - m) / (a * f0) + lat
        m = b * f0 * (
            (1 + nn + 5 / 4 * nn**2 + 5 / 4 * nn**3) * (lat - lat0)
            - (3 * nn + 3 * nn**2 + 21 / 8 * nn**3) * math.sin(lat - lat0) * math.cos(lat + lat0)
            + (15 / 8 * nn**2 + 15 / 8 * nn**3) * math.sin(2 * (lat - lat0)) * math.cos(2 * (lat + lat0))
            - 35 / 24 * nn**3 * math.sin(3 * (lat - lat0)) * math.cos(3 * (lat + lat0))
        )
        if abs(n - n0 - m) < 0.00001:
            break
    sin, cos, tan = math.sin(lat), math.cos(lat), math.tan(lat)
    nu = a * f0 / math.sqrt(1 - e2 * sin * sin)
    rho = a * f0 * (1 - e2) / (1 - e2 * sin * sin) ** 1.5
    eta2 = nu / rho - 1
    sec = 1 / cos
    vii = tan / (2 * rho * nu)
    viii = tan / (24 * rho * nu**3) * (5 + 3 * tan**2 + eta2 - 9 * tan**2 * eta2)
    ix = tan / (720 * rho * nu**5) * (61 + 90 * tan**2 + 45 * tan**4)
    x = sec / nu
    xi = sec / (6 * nu**3) * (nu / rho + 2 * tan**2)
    xii = sec / (120 * nu**5) * (5 + 28 * tan**2 + 24 * tan**4)
    xiia = sec / (5040 * nu**7) * (61 + 662 * tan**2 + 1320 * tan**4 + 720 * tan**6)
    de = e - e0
    phi = lat - vii * de**2 + viii * de**4 - ix * de**6
    lam = lon0 + x * de - xi * de**3 + xii * de**5 - xiia * de**7
    # To cartesian on Airy 1830, Helmert to WGS84, back to lat/lon on GRS80.
    nu = a / math.sqrt(1 - e2 * math.sin(phi) ** 2)
    x1 = nu * math.cos(phi) * math.cos(lam)
    y1 = nu * math.cos(phi) * math.sin(lam)
    z1 = (1 - e2) * nu * math.sin(phi)
    tx, ty, tz = 446.448, -125.157, 542.060
    s = -20.4894e-6
    rx, ry, rz = (math.radians(v / 3600) for v in (0.1502, 0.2470, 0.8421))
    x2 = tx + (1 + s) * x1 - rz * y1 + ry * z1
    y2 = ty + rz * x1 + (1 + s) * y1 - rx * z1
    z2 = tz - ry * x1 + rx * y1 + (1 + s) * z1
    a2, b2 = 6378137.0, 6356752.3141
    e22 = 1 - (b2 * b2) / (a2 * a2)
    p = math.hypot(x2, y2)
    phi = math.atan2(z2, p * (1 - e22))
    for _ in range(10):
        nu = a2 / math.sqrt(1 - e22 * math.sin(phi) ** 2)
        phi = math.atan2(z2 + e22 * nu * math.sin(phi), p)
    return math.degrees(phi), math.degrees(math.atan2(y2, x2))


def main():
    src, out = sys.argv[1], sys.argv[2]
    sums = {}
    with zipfile.ZipFile(src) as z:
        for name in sorted(z.namelist()):
            if not (name.startswith("Data/CSV/") and name.endswith(".csv")):
                continue
            for row in csv.reader(io.TextIOWrapper(z.open(name), encoding="latin-1")):
                postcode, quality, e, n = row[0], int(row[1]), int(row[2]), int(row[3])
                if quality >= 60 or (e == 0 and n == 0):
                    continue
                outcode = postcode[:-3].strip().upper()
                s = sums.setdefault(outcode, [0, 0, 0])
                s[0] += e
                s[1] += n
                s[2] += 1
    lines = []
    for outcode in sorted(sums):
        if outcode in NON_GEOGRAPHIC:
            continue
        e, n, count = sums[outcode]
        lat, lon = osgb36_to_wgs84(e / count, n / count)
        lines.append(f"{outcode},{lat:.4f},{lon:.4f}")
    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(len(lines), "outcodes")


if __name__ == "__main__":
    main()
