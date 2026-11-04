Shader "Fixture/Thing" {
    Properties { _MainTex ("Base", 2D) = "white" {} }
    SubShader {
        Tags { "RenderType"="Opaque" }
        CGPROGRAM
        #pragma surface surf Standard
        ENDCG
    }
}
