```diff
--- a/src/station.rs
+++ b/src/station.rs
@@ -12,6 +12,7 @@ fn read(sensor: &Sensor) -> Reading {
     let raw = sensor.sample();
-    let celsius = raw as f64 / 10.0;
-    Reading { celsius }
+    let celsius = raw as f64 / 10.0 + sensor.offset;
+    let humidity = sensor.humidity();
+    Reading { celsius, humidity }
 }
 
 fn log(reading: &Reading) {
```
