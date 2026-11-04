Shader "Fixture/Thing" {
    Properties { _MainTex ("Base", 2D) = "white" {} }
    SubShader {
        Tags { "RenderType"="TransparentCutout" }
        CGPROGRAM
        #pragma surface surf Standard
        ENDCG
    }
}
